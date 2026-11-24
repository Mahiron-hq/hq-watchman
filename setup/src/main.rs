// SPDX-License-Identifier: GPL-3.0-or-later
#![windows_subsystem = "windows"]

#[allow(
    unsafe_code,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::print_stdout
)]
mod ui {
    slint::include_modules!();
}

mod deploy;
mod local;
mod plan;
mod release;
mod services;
mod ssh;
mod store;

use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use hq_watchman::i18n::Lang;
use hq_watchman::settings::{Game, Settings};
use hq_watchman::{tr, trf};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use zeroize::Zeroizing;

use crate::deploy::{Log, ServerInfo};
use crate::plan::{GithubPlan, Plan, Proxy, Target};
use crate::ssh::Session;
use crate::ui::{Found, GameRow, MainWindow, ProfileRow, StepRow, ToggleRow, Tr, S};

const POLLING_HOOK_HOST: &str = "https://hq-watchman.invalid/";

#[derive(Default)]
struct Managed {
    target: Option<Target>,
    settings: Settings,
    repos: Vec<String>,
}

#[derive(Default)]
struct State {
    session: Option<Arc<Session>>,
    info: Option<ServerInfo>,
    github: Option<GithubPlan>,
    github_org: bool,
    steam_profile: Option<u64>,
    cancel: services::Cancel,
    pending_host_key: Option<(String, u16, String, &'static str)>,
    source: Option<release::Source>,
    managed: Managed,
    update: Option<release::Source>,
    log: String,
}

#[derive(Clone)]
struct Ctx {
    ui: slint::Weak<MainWindow>,
    rt: tokio::runtime::Handle,
    st: Arc<Mutex<State>>,
}

fn lock(st: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    st.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn err_text(e: &anyhow::Error) -> String {
    if let Some(h) = e.downcast_ref::<ssh::HostKeyError>() {
        return h.to_string();
    }
    format!("{e:#}")
}

impl Ctx {
    fn ui(&self, f: impl FnOnce(&MainWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| f(&ui));
    }

    fn info(&self, text: String) {
        self.ui(move |ui| ui.global::<S>().set_info(text.into()));
    }

    /// Runs async work with the busy indicator; errors go to the error line.
    fn task<F, Fut>(&self, f: F)
    where
        F: FnOnce(Ctx) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let c = self.clone();
        self.ui(|ui| {
            let s = ui.global::<S>();
            s.set_busy(true);
            s.set_error(SharedString::new());
            s.set_info(SharedString::new());
        });
        self.rt.spawn(async move {
            let result = f(c.clone()).await;
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_busy(false);
                if let Err(e) = result {
                    if let Some(h) = e.downcast_ref::<ssh::HostKeyError>() {
                        if !h.changed {
                            return;
                        }
                    }
                    s.set_error(err_text(&e).into());
                }
            });
        });
    }

    fn log_sink(&self) -> Log {
        let c = self.clone();
        Arc::new(move |line: String| {
            let text = {
                let mut st = lock(&c.st);
                st.log.push_str(&line);
                st.log.push('\n');
                st.log.clone()
            };
            c.ui(move |ui| ui.global::<S>().set_log(text.into()));
        })
    }
}

fn target_of(s: &S) -> Target {
    Target::from_code(&s.get_target()).unwrap_or(Target::Docker)
}

fn ssh_target(s: &S) -> Result<ssh::Target> {
    let host = s.get_host().trim().to_owned();
    if host.is_empty() || host.len() > 253 || host.contains(char::is_whitespace) {
        bail!(
            "{}",
            tr!("Укажите адрес сервера", "Enter the server address")
        );
    }
    let port: u16 = s
        .get_port()
        .trim()
        .parse()
        .map_err(|_| anyhow!("{}", tr!("Неверный порт", "Wrong port")))?;
    let user = s.get_user().trim().to_owned();
    if user.is_empty() {
        bail!("{}", tr!("Укажите пользователя", "Enter the user name"));
    }
    let auth = if s.get_auth_key() {
        let path = s.get_key_path().trim().to_owned();
        if path.is_empty() {
            bail!("{}", tr!("Выберите файл ключа", "Choose the key file"));
        }
        let pass = s.get_key_pass().to_string();
        ssh::Auth::Key {
            path,
            passphrase: (!pass.is_empty()).then(|| Zeroizing::new(pass)),
        }
    } else {
        let pw = s.get_password().to_string();
        if pw.is_empty() {
            bail!("{}", tr!("Введите пароль", "Enter the password"));
        }
        ssh::Auth::Password(Zeroizing::new(pw))
    };
    let sudo = s.get_sudo_pass().to_string();
    Ok(ssh::Target {
        host,
        port,
        user,
        auth,
        sudo_password: (!sudo.is_empty()).then(|| Zeroizing::new(sudo)),
    })
}

/// Connects with the pinned host key; an unknown key opens the confirmation dialog.
async fn open_session(c: &Ctx, t: &ssh::Target, action: &'static str) -> Result<Session> {
    let pinned = store::host_key(&store::load(), &t.host, t.port);
    match Session::connect(t, pinned).await {
        Ok(s) => Ok(s),
        Err(e) => {
            if let Some(h) = e.downcast_ref::<ssh::HostKeyError>() {
                if h.changed {
                    bail!(
                        "{}",
                        trf!(
                            "Ключ сервера изменился ({})! Возможна подмена сервера. Если сервер переустанавливали, удалите его из сохранённых и подключитесь заново.",
                            "The server key changed ({})! The server may be spoofed. If it was reinstalled, remove it from saved servers and connect again.",
                            h.fingerprint
                        )
                    );
                }
                lock(&c.st).pending_host_key =
                    Some((t.host.clone(), t.port, h.fingerprint.clone(), action));
                let text = trf!(
                    "Первое подключение к {}:{}. Отпечаток ключа сервера:\n{}\nЕсли сомневаетесь, сверьте его с данными хостинга. Доверять этому серверу?",
                    "First connection to {}:{}. Server key fingerprint:\n{}\nIf unsure, compare it with your hosting data. Trust this server?",
                    t.host,
                    t.port,
                    h.fingerprint
                );
                c.ui(move |ui| {
                    let s = ui.global::<S>();
                    s.set_hk_text(text.into());
                    s.set_hk_show(true);
                });
            }
            Err(e)
        }
    }
}

fn step_ids(s: &S) -> Vec<&'static str> {
    let mut v = vec!["target"];
    if target_of(s).is_remote() {
        v.push("server");
    }
    v.push("sources");
    v.push("telegram");
    if s.get_src_github() {
        v.push("github");
    }
    if s.get_src_guides() {
        v.push("steam");
    }
    if s.get_src_games() {
        v.push("games");
    }
    v.extend(["notify", "summary", "progress"]);
    v
}

fn step_label(id: &str) -> &'static str {
    match id {
        "target" => tr!("Где работать", "Where to run"),
        "server" => tr!("Сервер", "Server"),
        "sources" => tr!("Источники", "Sources"),
        "telegram" => "Telegram",
        "github" => "GitHub",
        "steam" => tr!("Руководства Steam", "Steam guides"),
        "games" => tr!("Игры Steam", "Steam games"),
        "notify" => tr!("Уведомления", "Notifications"),
        "summary" => tr!("Проверка", "Review"),
        "progress" => tr!("Установка", "Installation"),
        "connect" => tr!("Подключение", "Connection"),
        "manage" => tr!("Управление", "Management"),
        _ => "",
    }
}

fn refresh_steps(ui: &MainWindow) {
    let s = ui.global::<S>();
    let page = s.get_page();
    let ids: Vec<&str> = if page == "connect" || page == "manage" {
        vec!["connect", "manage"]
    } else if page == "welcome" {
        Vec::new()
    } else {
        step_ids(&s)
    };
    let rows: Vec<StepRow> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| StepRow {
            id: (*id).into(),
            label: format!("{}. {}", i + 1, step_label(id)).into(),
        })
        .collect();
    s.set_steps(ModelRc::new(VecModel::from(rows)));
}

fn show(ui: &MainWindow, page: &str) {
    let s = ui.global::<S>();
    s.set_page(page.into());
    s.set_error(SharedString::new());
    s.set_info(SharedString::new());
    refresh_steps(ui);
}

fn games_of(s: &S) -> Vec<(u32, String)> {
    s.get_games()
        .iter()
        .filter_map(|g| Some((g.appid.parse().ok()?, g.name.to_string())))
        .collect()
}

fn set_games(s: &S, list: &[(u32, String)]) {
    let rows: Vec<GameRow> = list
        .iter()
        .map(|(id, n)| GameRow {
            appid: id.to_string().into(),
            name: n.clone().into(),
        })
        .collect();
    s.set_games(ModelRc::new(VecModel::from(rows)));
}

fn valid_tz(tz: &str) -> bool {
    !tz.is_empty() && tz.len() <= 64 && jiff::tz::TimeZone::get(tz).is_ok()
}

/// Checks the current wizard page; returns an error text or nothing.
fn check_page(c: &Ctx, s: &S) -> Option<String> {
    let st = lock(&c.st);
    match s.get_page().as_str() {
        "server" if !s.get_server_ok() => Some(
            tr!(
                "Сначала проверьте подключение к серверу",
                "Test the server connection first"
            )
            .into(),
        ),
        "sources" if !s.get_src_github() && !s.get_src_guides() && !s.get_src_games() => Some(
            tr!(
                "Выберите хотя бы один источник",
                "Choose at least one source"
            )
            .into(),
        ),
        "telegram" => {
            if s.get_tg_bot().is_empty() {
                Some(tr!("Проверьте токен бота", "Check the bot token").into())
            } else if s
                .get_tg_admin()
                .trim()
                .parse::<i64>()
                .map_or(true, |v| v <= 0)
            {
                Some(
                    tr!(
                        "Нужен ваш ID Telegram: нажмите Start в боте",
                        "Your Telegram ID is needed: press Start in the bot"
                    )
                    .into(),
                )
            } else {
                None
            }
        }
        "github" if !(st.github.is_some() && s.get_gh_installed()) => Some(
            tr!(
                "Создайте GitHub App и установите его на аккаунт",
                "Create the GitHub App and install it on the account"
            )
            .into(),
        ),
        "steam" if st.steam_profile.is_none() => Some(
            tr!(
                "Проверьте ссылку на профиль Steam",
                "Check the Steam profile link"
            )
            .into(),
        ),
        "games" if s.get_games().row_count() == 0 => {
            Some(tr!("Добавьте хотя бы одну игру", "Add at least one game").into())
        }
        "notify" => {
            if !valid_tz(s.get_timezone().trim()) {
                Some(tr!("Неизвестный часовой пояс", "Unknown time zone").into())
            } else if hq_watchman::settings::parse_hhmm(&s.get_quiet_start()).is_none()
                || hq_watchman::settings::parse_hhmm(&s.get_quiet_end()).is_none()
            {
                Some(
                    tr!(
                        "Время тихих часов в виде 22:00",
                        "Quiet hours time like 22:00"
                    )
                    .into(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

fn build_plan(c: &Ctx, s: &S) -> Result<Plan> {
    let st = lock(&c.st);
    let target = target_of(s);
    let github = if s.get_src_github() {
        st.github.clone()
    } else {
        None
    };
    let proxy = match (&github, &st.info) {
        (Some(g), Some(info)) if g.webhook => {
            if info.nginx {
                Some(Proxy::Nginx)
            } else if info.busy_web_ports && info.installed.as_deref() != Some("docker") {
                bail!(
                    "{}",
                    tr!(
                        "Порты 80/443 на сервере заняты другой программой. Освободите их или выберите для GitHub режим опроса.",
                        "Ports 80/443 on the server are used by another program. Free them or choose polling mode for GitHub."
                    )
                );
            } else {
                Some(Proxy::Caddy)
            }
        }
        _ => None,
    };
    let games: Vec<Game> = if s.get_src_games() {
        games_of(s)
            .into_iter()
            .map(|(id, n)| Game::new(id, &n))
            .collect()
    } else {
        Vec::new()
    };
    let key = s.get_st_key().trim().to_owned();
    let plan = Plan {
        target,
        language: if s.get_bot_en() { Lang::En } else { Lang::Ru },
        github_on: s.get_src_github(),
        guides_on: s.get_src_guides(),
        games_on: s.get_src_games(),
        telegram_token: Zeroizing::new(s.get_tg_token().trim().to_owned()),
        telegram_admin: s.get_tg_admin().trim().parse().unwrap_or(0),
        github,
        steam_profile: if s.get_src_guides() {
            st.steam_profile
        } else {
            None
        },
        steam_key: (s.get_src_guides() && !key.is_empty()).then(|| Zeroizing::new(key)),
        games,
        timezone: s.get_timezone().trim().to_owned(),
        quiet_enabled: s.get_quiet_on(),
        quiet_start: s.get_quiet_start().trim().to_owned(),
        quiet_end: s.get_quiet_end().trim().to_owned(),
        proxy,
    };
    plan.validate()?;
    Ok(plan)
}

fn summary(plan: &Plan, s: &S) -> String {
    let yes = |b: bool| {
        if b {
            tr!("да", "yes")
        } else {
            tr!("нет", "no")
        }
    };
    let target = match plan.target {
        Target::Docker => tr!("сервер, Docker", "server, Docker"),
        Target::Systemd => tr!("сервер, служба systemd", "server, systemd service"),
        Target::Windows => tr!("этот компьютер", "this computer"),
    };
    let mut out = trf!("Где: {target}", "Where: {target}");
    if plan.target.is_remote() {
        let _ = write!(
            out,
            "\n{}: {}@{}",
            tr!("Сервер", "Server"),
            s.get_user(),
            s.get_host()
        );
    }
    let _ = write!(
        out,
        "\nTelegram: @{} · ID {}",
        s.get_tg_bot(),
        plan.telegram_admin
    );
    let gh = match &plan.github {
        Some(g) if g.webhook => trf!("вебхук на {}", "webhook at {}", g.domain),
        Some(_) => tr!("опрос", "polling").to_owned(),
        None => tr!("нет", "no").to_owned(),
    };
    let _ = write!(out, "\nGitHub: {gh}");
    let _ = write!(
        out,
        "\n{}: {}",
        tr!("Руководства Steam", "Steam guides"),
        if plan.steam_profile.is_some() {
            s.get_st_profile_name().to_string()
        } else {
            yes(false).to_owned()
        }
    );
    let _ = write!(
        out,
        "\n{}: {}",
        tr!("Игры Steam", "Steam games"),
        plan.games.len()
    );
    let _ = write!(
        out,
        "\n{}: {} · {}",
        tr!("Язык и часовой пояс", "Language and time zone"),
        plan.language.code(),
        plan.timezone
    );
    if plan.quiet_enabled {
        let _ = write!(
            out,
            "\n{}: {}-{}",
            tr!("Тихие часы", "Quiet hours"),
            plan.quiet_start,
            plan.quiet_end
        );
    }
    out
}

fn wire_wizard(ui: &MainWindow, c: &Ctx) {
    let s = ui.global::<S>();

    let w = ui.as_weak();
    s.on_go(move |page| {
        if let Some(ui) = w.upgrade() {
            show(&ui, &page);
        }
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_next(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        if let Some(e) = check_page(&cc, &s) {
            s.set_error(e.into());
            return;
        }
        let ids = step_ids(&s);
        let page = s.get_page();
        let Some(i) = ids.iter().position(|p| *p == page.as_str()) else {
            return;
        };
        let Some(next) = ids.get(i + 1) else { return };
        if *next == "summary" {
            match build_plan(&cc, &s) {
                Ok(plan) => s.set_summary(summary(&plan, &s).into()),
                Err(e) => {
                    s.set_error(err_text(&e).into());
                    return;
                }
            }
        }
        show(&ui, next);
    });

    let w = ui.as_weak();
    s.on_back(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let ids = step_ids(&s);
        let page = s.get_page();
        match ids.iter().position(|p| *p == page.as_str()) {
            Some(0) | None => show(&ui, "welcome"),
            Some(i) => show(&ui, ids.get(i - 1).copied().unwrap_or("welcome")),
        }
    });

    s.on_open_url(|url| {
        let _ = open::that(url.as_str());
    });

    let w = ui.as_weak();
    s.on_set_lang(move |en| {
        let Some(ui) = w.upgrade() else { return };
        ui.global::<Tr>().set_en(en);
        hq_watchman::i18n::set(if en { Lang::En } else { Lang::Ru });
        store::set_language(if en { "en" } else { "ru" });
        refresh_steps(&ui);
    });

    let cc = c.clone();
    s.on_pick_key(move || {
        let c = cc.clone();
        cc.rt.spawn(async move {
            if let Some(f) = rfd::AsyncFileDialog::new().pick_file().await {
                let p = f.path().display().to_string();
                c.ui(move |ui| ui.global::<S>().set_key_path(p.into()));
            }
        });
    });

    let cc = c.clone();
    s.on_pick_folder(move || {
        cc.task(|c| async move {
            let Some(dir) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return Ok(());
            };
            let src = release::folder(dir.path().to_path_buf());
            let bundle = release::Bundle::open(src.clone()).await?;
            let text = trf!(
                "папка {}, версия {}, подпись верна",
                "folder {}, version {}, signature valid",
                dir.path().display(),
                bundle.version
            );
            {
                let mut st = lock(&c.st);
                st.source = Some(src.clone());
                st.update = Some(src);
            }
            let v = bundle.version.clone();
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_release_source(text.into());
                s.set_update_text(
                    trf!(
                        "Готова версия {v} из папки.",
                        "Version {v} from the folder is ready."
                    )
                    .into(),
                );
                s.set_update_ready(true);
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_server_test(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        s.set_server_ok(false);
        s.set_server_status(SharedString::new());
        let t = match ssh_target(&s) {
            Ok(t) => t,
            Err(e) => return s.set_error(err_text(&e).into()),
        };
        let target = target_of(&s);
        cc.task(move |c| async move {
            let session = open_session(&c, &t, "server").await?;
            session.check_root().await?;
            let info = deploy::inspect(&session).await?;
            deploy::supported(&info)?;
            let yes = |b: bool| {
                if b {
                    tr!("да", "yes")
                } else {
                    tr!("нет", "no")
                }
            };
            let mut text = trf!(
                "Подключено: {}, {}. Docker: {}. nginx: {}.",
                "Connected: {}, {}. Docker: {}. nginx: {}.",
                info.pretty,
                info.arch,
                yes(info.docker),
                yes(info.nginx)
            );
            if let Some(i) = &info.installed {
                text.push_str(&trf!(
                    " Найдена установка ({i}): она будет заменена, данные бота сохранятся.",
                    " An installation ({i}) was found: it will be replaced, bot data is kept."
                ));
            }
            let _ = store::remember(store::Profile {
                name: t.host.clone(),
                target: target.code().into(),
                host: t.host.clone(),
                port: t.port,
                user: t.user.clone(),
                key_path: match &t.auth {
                    ssh::Auth::Key { path, .. } => path.clone(),
                    ssh::Auth::Password(_) => String::new(),
                },
            });
            {
                let mut st = lock(&c.st);
                st.session = Some(Arc::new(session));
                st.info = Some(info);
            }
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_server_status(text.into());
                s.set_server_ok(true);
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_hk_accept(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        s.set_hk_show(false);
        let pending = lock(&cc.st).pending_host_key.take();
        if let Some((host, port, fp, action)) = pending {
            if let Err(e) = store::pin_host(&host, port, &fp) {
                s.set_error(err_text(&e).into());
                return;
            }
            match action {
                "server" => s.invoke_server_test(),
                _ => s.invoke_connect(),
            }
        }
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_hk_reject(move || {
        lock(&cc.st).pending_host_key = None;
        if let Some(ui) = w.upgrade() {
            ui.global::<S>().set_hk_show(false);
        }
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_tg_check(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        s.set_tg_bot(SharedString::new());
        let token = s.get_tg_token().trim().to_owned();
        cc.task(move |c| async move {
            let name = services::telegram_bot(&token).await?;
            c.ui(move |ui| ui.global::<S>().set_tg_bot(name.into()));
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_tg_wait(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let token = s.get_tg_token().trim().to_owned();
        let cancel: services::Cancel = Arc::new(AtomicBool::new(false));
        lock(&cc.st).cancel = Arc::clone(&cancel);
        s.set_tg_waiting(true);
        s.set_error(SharedString::new());
        let c = cc.clone();
        cc.rt.spawn(async move {
            let r = services::telegram_wait_start(&token, &cancel).await;
            if let Ok((id, _)) = &r {
                let _ = services::telegram_send(
                    &token,
                    *id,
                    tr!(
                        "HQ Watchman Setup: ваш ID получен, продолжайте установку.",
                        "HQ Watchman Setup: your ID was received, continue the setup."
                    ),
                )
                .await;
            }
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_tg_waiting(false);
                match r {
                    Ok((id, name)) => {
                        s.set_tg_admin(id.to_string().into());
                        s.set_tg_admin_name(name.into());
                    }
                    Err(e) if e.to_string() == "cancelled" => {}
                    Err(e) => s.set_error(err_text(&e).into()),
                }
            });
        });
    });

    let cc = c.clone();
    s.on_tg_cancel(move || {
        lock(&cc.st).cancel.store(true, Ordering::Relaxed);
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_gh_create(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let login = s.get_gh_login().trim().to_owned();
        let webhook = s.get_gh_webhook();
        let domain = s.get_gh_domain().trim().to_lowercase();
        if webhook && !plan::valid_domain(&domain) {
            s.set_error(
                tr!(
                    "Укажите домен, например hooks.example.org",
                    "Enter a domain, e.g. hooks.example.org"
                )
                .into(),
            );
            return;
        }
        let cancel: services::Cancel = Arc::new(AtomicBool::new(false));
        lock(&cc.st).cancel = Arc::clone(&cancel);
        cc.task(move |c| async move {
            let (user_id, login, org) = services::github_user(&login).await?;
            let path = plan::random_token(32);
            let hook = if webhook {
                format!("https://{domain}/{path}")
            } else {
                format!("{POLLING_HOOK_HOST}{path}")
            };
            c.info(
                tr!(
                    "Браузер открыт: нажмите «Create GitHub App» на GitHub.",
                    "The browser is open: press \"Create GitHub App\" on GitHub."
                )
                .into(),
            );
            let app = services::github_create_app(
                &login,
                org,
                &hook,
                &cancel,
                tr!(
                    "GitHub App создано. Вернитесь в HQ Watchman Setup.",
                    "GitHub App created. Return to HQ Watchman Setup."
                ),
            )
            .await?;
            let secret = if app.webhook_secret.len() >= 32 {
                app.webhook_secret.clone()
            } else {
                Zeroizing::new(plan::random_token(48))
            };
            let slug = app.slug.clone();
            {
                let mut st = lock(&c.st);
                st.github = Some(GithubPlan {
                    app_id: app.id,
                    slug: app.slug,
                    pem: app.pem,
                    webhook_secret: secret,
                    user_id,
                    webhook,
                    domain,
                    hook_path: path,
                });
                st.github_org = org;
            }
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_gh_app(slug.into());
                s.set_gh_login(login.into());
                s.set_info(SharedString::new());
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_gh_install(move || {
        let Some(ui) = w.upgrade() else { return };
        let Some(g) = lock(&cc.st).github.clone() else {
            return;
        };
        let s = ui.global::<S>();
        let _ = open::that(format!(
            "https://github.com/apps/{}/installations/new",
            g.slug
        ));
        s.set_gh_waiting(true);
        let cancel: services::Cancel = Arc::new(AtomicBool::new(false));
        lock(&cc.st).cancel = Arc::clone(&cancel);
        cc.task(move |c| async move {
            c.info(
                tr!(
                    "Ждём установки App на GitHub…",
                    "Waiting for the App installation on GitHub…"
                )
                .into(),
            );
            let r = services::github_wait_install(g.app_id, &g.pem, g.user_id, &cancel).await;
            let ok = r.is_ok();
            c.ui(move |ui| {
                let s = ui.global::<S>();
                s.set_gh_waiting(false);
                s.set_info(SharedString::new());
                if ok {
                    s.set_gh_installed(true);
                }
            });
            r
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_st_check(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        s.set_st_profile_name(SharedString::new());
        let input = s.get_st_profile().to_string();
        cc.task(move |c| async move {
            let (id, name) = services::steam_profile(&input).await?;
            lock(&c.st).steam_profile = Some(id);
            c.ui(move |ui| {
                ui.global::<S>()
                    .set_st_profile_name(format!("{name} ({id})").into());
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_st_key_check(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        s.set_st_key_status(SharedString::new());
        let key = s.get_st_key().to_string();
        let Some(profile) = lock(&cc.st).steam_profile else {
            s.set_error(tr!("Сначала проверьте профиль", "Check the profile first").into());
            return;
        };
        cc.task(move |c| async move {
            services::steam_key(&key, profile).await?;
            c.ui(|ui| {
                ui.global::<S>()
                    .set_st_key_status(tr!("Ключ работает", "The key works").into());
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_game_search(move || {
        let Some(ui) = w.upgrade() else { return };
        let q = ui.global::<S>().get_game_query().trim().to_owned();
        if q.is_empty() {
            return;
        }
        cc.task(move |c| async move {
            let http = services::http()?;
            let found = if let Some(id) = hq_watchman::steam::store::parse_appid(&q) {
                let name = hq_watchman::steam::store::name(&http, id)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "{}",
                            tr!(
                                "Steam не знает такой AppID",
                                "Steam does not know this AppID"
                            )
                        )
                    })?;
                vec![(id, name)]
            } else {
                hq_watchman::steam::store::search(&http, &q)
                    .await?
                    .into_iter()
                    .map(|f| (f.appid, f.name))
                    .collect()
            };
            if found.is_empty() {
                bail!("{}", tr!("Ничего не найдено", "Nothing found"));
            }
            c.ui(move |ui| {
                let rows: Vec<Found> = found
                    .into_iter()
                    .map(|(id, n)| Found {
                        appid: id.to_string().into(),
                        name: n.into(),
                    })
                    .collect();
                ui.global::<S>()
                    .set_found(ModelRc::new(VecModel::from(rows)));
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    s.on_game_add(move |id, name| {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let mut list = games_of(&s);
        let Ok(id) = id.parse::<u32>() else { return };
        if list.iter().any(|(x, _)| *x == id) {
            return;
        }
        if list.len() >= hq_watchman::settings::MAX_GAMES {
            s.set_error(tr!("Не больше 50 игр", "At most 50 games").into());
            return;
        }
        list.push((id, name.to_string()));
        set_games(&s, &list);
        s.set_found(ModelRc::new(VecModel::from(Vec::<Found>::new())));
        s.set_game_query(SharedString::new());
    });

    let w = ui.as_weak();
    s.on_game_remove(move |id| {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let list: Vec<(u32, String)> = games_of(&s)
            .into_iter()
            .filter(|(x, _)| x.to_string() != id.as_str())
            .collect();
        set_games(&s, &list);
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_install(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let plan = match build_plan(&cc, &s) {
            Ok(p) => p,
            Err(e) => return s.set_error(err_text(&e).into()),
        };
        lock(&cc.st).log.clear();
        s.set_log(SharedString::new());
        s.set_done(false);
        s.set_failed(false);
        show(&ui, "progress");
        let host = s.get_host().to_string();
        let tg_bot = s.get_tg_bot().to_string();
        cc.task(move |c| async move {
            let log = c.log_sink();
            let r = run_install(&c, &plan, &log, &host).await;
            match &r {
                Ok(()) => {
                    log(tr!("Готово.", "Done.").into());
                    let _ = tg_bot;
                    c.ui(|ui| ui.global::<S>().set_done(true));
                }
                Err(e) => {
                    log(format!("! {}", err_text(e)));
                    c.ui(|ui| ui.global::<S>().set_failed(true));
                }
            }
            r
        });
    });
}

async fn run_install(c: &Ctx, plan: &Plan, log: &Log, host: &str) -> Result<()> {
    let source = lock(&c.st).source.clone();
    let source = if let Some(s) = source {
        s
    } else {
        log(format!(
            "› {}",
            tr!(
                "Поиск последнего релиза на GitHub",
                "Looking up the latest release on GitHub"
            )
        ));
        release::latest().await?
    };
    let bundle = release::Bundle::open(source).await?;
    log(format!("  {} {}", tr!("версия", "version"), bundle.version));
    if plan.target == Target::Windows {
        return local::install(plan, &bundle, log).await;
    }
    let (session, info) = {
        let st = lock(&c.st);
        (st.session.clone(), st.info.clone())
    };
    let (Some(session), Some(info)) = (session, info) else {
        bail!(
            "{}",
            tr!("Нет подключения к серверу", "No server connection")
        );
    };
    match plan.target {
        Target::Docker => deploy::install_docker(&session, &info, plan, &bundle, log).await?,
        _ => deploy::install_systemd(&session, &info, plan, &bundle, log).await?,
    }
    if let Some(g) = plan.github.as_ref().filter(|g| g.webhook) {
        log(format!(
            "› {}",
            tr!(
                "Проверка адреса вебхука снаружи",
                "Checking the webhook address from outside"
            )
        ));
        let url = format!("https://{}/{}", g.domain, g.hook_path);
        match reqwest::Client::new().get(&url).send().await {
            Ok(r) if r.status().as_u16() == 404 => log(format!(
                "  {}",
                tr!("адрес доступен, HTTPS работает", "reachable, HTTPS works")
            )),
            Ok(r) => log(format!(
                "  {} {}",
                tr!("неожиданный ответ", "unexpected answer"),
                r.status()
            )),
            Err(e) => log(format!(
                "  {}: {e}. {}",
                tr!("адрес недоступен", "not reachable"),
                tr!(
                    "Проверьте, что домен указывает на сервер и порты 80/443 открыты.",
                    "Check that the domain points to the server and ports 80/443 are open."
                )
            )),
        }
    }
    let _ = host;
    Ok(())
}

fn toggle_rows(m: &Managed) -> Vec<ToggleRow> {
    let s = &m.settings;
    let mut v = Vec::new();
    let head = |v: &mut Vec<ToggleRow>, label: &str| {
        v.push(ToggleRow {
            key: SharedString::new(),
            label: label.into(),
            on: false,
            header: true,
            action: SharedString::new(),
        });
    };
    let row = |v: &mut Vec<ToggleRow>, key: String, label: String, on: bool| {
        v.push(ToggleRow {
            key: key.into(),
            label: label.into(),
            on,
            header: false,
            action: SharedString::new(),
        });
    };
    let g = &s.github;
    head(&mut v, "GitHub");
    for (k, l, on) in [
        (
            "gh.on",
            tr!("Уведомления GitHub", "GitHub notifications"),
            g.enabled,
        ),
        ("gh.issues", "Issues", g.issues),
        ("gh.prs", "Pull requests", g.pull_requests),
        ("gh.disc", tr!("Обсуждения", "Discussions"), g.discussions),
        ("gh.com", tr!("Комментарии", "Comments"), g.comments),
        ("gh.rev", tr!("Ревью", "Reviews"), g.reviews),
        (
            "gh.revc",
            tr!("Комментарии к коду", "Code comments"),
            g.review_comments,
        ),
        (
            "gh.commit",
            tr!("Комментарии к коммитам", "Commit comments"),
            g.commit_comments,
        ),
        (
            "gh.repos",
            tr!("Новые репозитории", "New repositories"),
            g.repositories,
        ),
        (
            "gh.edits",
            tr!("Изменения комментариев", "Comment edits"),
            g.edits,
        ),
        (
            "gh.dels",
            tr!("Удаления комментариев", "Comment deletions"),
            g.deletes,
        ),
    ] {
        row(&mut v, k.into(), l.into(), on);
    }
    if !m.repos.is_empty() {
        head(
            &mut v,
            tr!(
                "Репозитории (выключенные не присылаются)",
                "Repositories (switched off ones are not sent)"
            ),
        );
        for r in &m.repos {
            let on = !g.excluded_repos.iter().any(|x| x.eq_ignore_ascii_case(r));
            row(&mut v, format!("repo:{r}"), r.clone(), on);
        }
    }
    head(&mut v, tr!("Руководства Steam", "Steam guides"));
    let gd = &s.guides;
    for (k, l, on) in [
        (
            "gd.on",
            tr!("Уведомления о руководствах", "Guide notifications"),
            gd.enabled,
        ),
        (
            "gd.com",
            tr!("Новые комментарии", "New comments"),
            gd.comments,
        ),
        (
            "gd.del",
            tr!("Удалённые комментарии", "Deleted comments"),
            gd.deleted_comments,
        ),
        (
            "gd.list",
            tr!("Новые и пропавшие руководства", "New and removed guides"),
            gd.guide_changes,
        ),
    ] {
        row(&mut v, k.into(), l.into(), on);
    }
    head(&mut v, tr!("Игры Steam", "Steam games"));
    row(
        &mut v,
        "gm.on".into(),
        tr!("Уведомления об играх", "Game notifications").into(),
        s.games.enabled,
    );
    for gm in &s.games.list {
        let n = gm.label();
        let id = gm.appid;
        for (k, l, on) in [
            (
                "b",
                tr!("обновления (public)", "updates (public)"),
                gm.builds,
            ),
            ("br", tr!("бета-ветки", "beta branches"), gm.branches),
            ("d", "DLC", gm.dlc),
            ("n", tr!("новости", "news"), gm.news),
            ("p", tr!("патчноуты", "patch notes"), gm.patches),
        ] {
            row(&mut v, format!("g:{id}:{k}"), format!("{n}: {l}"), on);
        }
    }
    head(&mut v, tr!("Куда слать", "Delivery"));
    let a = s.admin;
    for (k, l, on) in [
        (
            "a:gh",
            tr!("Личный чат: GitHub", "Private chat: GitHub"),
            a.github,
        ),
        (
            "a:gd",
            tr!("Личный чат: руководства", "Private chat: guides"),
            a.guides,
        ),
        (
            "a:gm",
            tr!("Личный чат: игры", "Private chat: games"),
            a.games,
        ),
    ] {
        row(&mut v, k.into(), l.into(), on);
    }
    for ch in &s.chats {
        let d = ch.delivery;
        let id = ch.id;
        for (k, l, on) in [
            ("gh", "GitHub", d.github),
            ("gd", tr!("руководства", "guides"), d.guides),
            ("gm", tr!("игры", "games"), d.games),
        ] {
            row(
                &mut v,
                format!("c:{id}:{k}"),
                format!("{}: {l}", ch.title),
                on,
            );
        }
        v.push(ToggleRow {
            key: format!("forget:{id}").into(),
            label: trf!(
                "{}: убрать из рассылки",
                "{}: remove from delivery",
                ch.title
            )
            .into(),
            on: false,
            header: false,
            action: "forget".into(),
        });
    }
    v
}

fn show_managed(ui: &MainWindow, m: &Managed) {
    let s = ui.global::<S>();
    s.set_toggles(ModelRc::new(VecModel::from(toggle_rows(m))));
    let games: Vec<(u32, String)> = m
        .settings
        .games
        .list
        .iter()
        .map(|g| (g.appid, g.name.clone()))
        .collect();
    set_games(&s, &games);
    s.set_bot_en(m.settings.language == Lang::En);
    s.set_timezone(m.settings.timezone.clone().into());
    s.set_quiet_on(m.settings.quiet_hours.enabled);
    s.set_quiet_start(m.settings.quiet_hours.start.clone().into());
    s.set_quiet_end(m.settings.quiet_hours.end.clone().into());
}

async fn load_managed(c: &Ctx) -> Result<()> {
    let (target, session) = {
        let st = lock(&c.st);
        (st.managed.target, st.session.clone())
    };
    let target = target.ok_or_else(|| anyhow!("not connected"))?;
    let (v, status) = if target == Target::Windows {
        let v = tokio::task::spawn_blocking(local::settings_get).await??;
        let ok = hq_watchman::app::healthcheck("127.0.0.1:9091")
            .await
            .is_ok();
        let st = trf!(
            "Этот компьютер. Состояние: {}. {}",
            "This computer. State: {}. {}",
            if ok {
                tr!("работает", "running")
            } else {
                tr!("не отвечает", "not responding")
            },
            local::version()
        );
        (v, st)
    } else {
        let session = session.ok_or_else(|| anyhow!("not connected"))?;
        let v = deploy::settings_get(&session, target).await?;
        let st = deploy::status(&session, target).await?;
        let info = lock(&c.st).info.clone().unwrap_or_default();
        let text = trf!(
            "Сервер: {} ({}, {}).\nУстановка: {}.\nСостояние: {}.\nВерсия бота: {}.",
            "Server: {} ({}, {}).\nInstallation: {}.\nState: {}.\nBot version: {}.",
            info.ip,
            info.pretty,
            info.arch,
            target.code(),
            st.state,
            st.version
        );
        (v, text)
    };
    let settings: Settings = serde_json::from_value(v["settings"].clone())?;
    let repos: Vec<String> = v["repos"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    {
        let mut st = lock(&c.st);
        st.managed.settings = settings;
        st.managed.repos = repos;
    }
    let c2 = c.clone();
    c.ui(move |ui| {
        let st = lock(&c2.st);
        show_managed(ui, &st.managed);
        ui.global::<S>().set_status_text(status.into());
    });
    Ok(())
}

fn wire_manage(ui: &MainWindow, c: &Ctx) {
    let s = ui.global::<S>();

    let w = ui.as_weak();
    s.on_use_profile(move |i| {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let Some(p) = usize::try_from(i)
            .ok()
            .and_then(|i| s.get_profiles().row_data(i))
        else {
            return;
        };
        s.set_host(p.host);
        s.set_port(p.port);
        s.set_user(p.user);
        s.set_target(p.target);
        s.set_auth_key(!p.key_path.is_empty());
        s.set_key_path(p.key_path);
    });

    let w = ui.as_weak();
    s.on_forget_profile(move |i| {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        if let Some(p) = usize::try_from(i)
            .ok()
            .and_then(|i| s.get_profiles().row_data(i))
        {
            let port = p.port.parse().unwrap_or(22);
            let _ = store::forget(&p.host, port);
            let mut d = store::load();
            d.known_hosts.remove(&format!("{}:{port}", p.host));
            let _ = store::save(&d);
            fill_profiles(&s);
        }
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_connect(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let local_mode = target_of(&s) == Target::Windows;
        let t = if local_mode {
            None
        } else {
            match ssh_target(&s) {
                Ok(t) => Some(t),
                Err(e) => return s.set_error(err_text(&e).into()),
            }
        };
        let w2 = w.clone();
        cc.task(move |c| async move {
            if let Some(t) = t {
                let session = open_session(&c, &t, "connect").await?;
                session.check_root().await?;
                let info = deploy::inspect(&session).await?;
                let target = match info.installed.as_deref() {
                    Some("docker") => Target::Docker,
                    Some("systemd") => Target::Systemd,
                    _ => bail!(
                        "{}",
                        tr!(
                            "На этом сервере бот не установлен",
                            "The bot is not installed on this server"
                        )
                    ),
                };
                let _ = store::remember(store::Profile {
                    name: t.host.clone(),
                    target: target.code().into(),
                    host: t.host.clone(),
                    port: t.port,
                    user: t.user.clone(),
                    key_path: match &t.auth {
                        ssh::Auth::Key { path, .. } => path.clone(),
                        ssh::Auth::Password(_) => String::new(),
                    },
                });
                let mut st = lock(&c.st);
                st.session = Some(Arc::new(session));
                st.info = Some(info);
                st.managed.target = Some(target);
            } else {
                if !local::installed() {
                    bail!(
                        "{}",
                        tr!(
                            "На этом компьютере бот не установлен",
                            "The bot is not installed on this computer"
                        )
                    );
                }
                lock(&c.st).managed.target = Some(Target::Windows);
            }
            load_managed(&c).await?;
            let _ = w2.upgrade_in_event_loop(|ui| {
                ui.global::<S>().set_tab("status".into());
                show(&ui, "manage");
            });
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_refresh(move || cc.task(|c| async move { load_managed(&c).await }));

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_toggle(move |key| {
        let Some(ui) = w.upgrade() else { return };
        let mut st = lock(&cc.st);
        let m = &mut st.managed;
        if let Some(repo) = key.strip_prefix("repo:") {
            let list = &mut m.settings.github.excluded_repos;
            if let Some(p) = list.iter().position(|r| r.eq_ignore_ascii_case(repo)) {
                list.remove(p);
            } else {
                list.push(repo.to_owned());
            }
        } else {
            let _ = hq_watchman::telegram::menu::toggle(&mut m.settings, &key);
        }
        let rows = toggle_rows(m);
        drop(st);
        ui.global::<S>()
            .set_toggles(ModelRc::new(VecModel::from(rows)));
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_row_action(move |key| {
        let Some(ui) = w.upgrade() else { return };
        let mut st = lock(&cc.st);
        if let Some(id) = key
            .strip_prefix("forget:")
            .and_then(|i| i.parse::<i64>().ok())
        {
            st.managed.settings.chats.retain(|c| c.id != id);
        }
        let rows = toggle_rows(&st.managed);
        drop(st);
        ui.global::<S>()
            .set_toggles(ModelRc::new(VecModel::from(rows)));
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_save_settings(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let tz = s.get_timezone().trim().to_owned();
        if !valid_tz(&tz) {
            s.set_error(tr!("Неизвестный часовой пояс", "Unknown time zone").into());
            return;
        }
        let settings = {
            let mut st = lock(&cc.st);
            let m = &mut st.managed.settings;
            let games = games_of(&s);
            let old = std::mem::take(&mut m.games.list);
            m.games.list = games
                .into_iter()
                .map(|(id, n)| {
                    old.iter()
                        .find(|g| g.appid == id)
                        .cloned()
                        .unwrap_or_else(|| Game::new(id, &n))
                })
                .collect();
            m.language = if s.get_bot_en() { Lang::En } else { Lang::Ru };
            m.timezone = tz;
            m.quiet_hours.enabled = s.get_quiet_on();
            s.get_quiet_start()
                .trim()
                .clone_into(&mut m.quiet_hours.start);
            s.get_quiet_end().trim().clone_into(&mut m.quiet_hours.end);
            m.clone()
        };
        cc.task(move |c| async move {
            settings.validate()?;
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            match (target, session) {
                (Some(Target::Windows), _) => {
                    let s2 = settings.clone();
                    tokio::task::spawn_blocking(move || local::settings_set(&s2)).await??;
                }
                (Some(t), Some(sess)) => deploy::settings_set(&sess, t, &settings).await?,
                _ => bail!("not connected"),
            }
            c.info(
                tr!(
                    "Сохранено. Бот применит настройки в течение нескольких секунд.",
                    "Saved. The bot applies the settings within a few seconds."
                )
                .into(),
            );
            let c2 = c.clone();
            c.ui(move |ui| {
                let st = lock(&c2.st);
                ui.global::<S>()
                    .set_toggles(ModelRc::new(VecModel::from(toggle_rows(&st.managed))));
            });
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_load_logs(move || {
        cc.task(|c| async move {
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            let text = match (target, session) {
                (Some(Target::Windows), _) => local::logs(300),
                (Some(t), Some(s)) => deploy::logs(&s, t, 300).await?,
                _ => String::new(),
            };
            c.ui(move |ui| ui.global::<S>().set_logs(text.into()));
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_check_update(move || {
        cc.task(|c| async move {
            let src = release::latest().await?;
            let bundle = release::Bundle::open(src.clone()).await?;
            let current = installed_version(&c).await;
            let newer = release::newer(&bundle.version, &current);
            let v = bundle.version.clone();
            lock(&c.st).update = Some(src);
            c.ui(move |ui| {
                let s = ui.global::<S>();
                if newer {
                    s.set_update_text(
                        trf!(
                            "Доступна версия {v} (установлена {current}).",
                            "Version {v} is available (installed {current})."
                        )
                        .into(),
                    );
                } else {
                    s.set_update_text(
                        trf!(
                            "Установлена последняя версия ({current}).",
                            "The latest version is installed ({current})."
                        )
                        .into(),
                    );
                }
                s.set_update_ready(newer);
            });
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_apply_update(move || {
        cc.task(|c| async move {
            let src = lock(&c.st)
                .update
                .clone()
                .ok_or_else(|| anyhow!("no update"))?;
            let bundle = release::Bundle::open(src).await?;
            let c2 = c.clone();
            let log: Log = Arc::new(move |line: String| {
                c2.ui(move |ui| {
                    let s = ui.global::<S>();
                    let mut t = s.get_update_text().to_string();
                    t.push('\n');
                    t.push_str(&line);
                    s.set_update_text(t.into());
                });
            });
            let (target, session, info) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone(), st.info.clone())
            };
            match (target, session, info) {
                (Some(Target::Windows), _, _) => local::update(&bundle, &log).await?,
                (Some(t), Some(s), Some(i)) => deploy::update(&s, &i, t, &bundle, &log).await?,
                _ => bail!("not connected"),
            }
            log(tr!("Обновлено.", "Updated.").into());
            c.ui(|ui| ui.global::<S>().set_update_ready(false));
            load_managed(&c).await
        });
    });

    let cc = c.clone();
    s.on_restart(move || {
        cc.task(|c| async move {
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            match (target, session) {
                (Some(Target::Windows), _) => tokio::task::spawn_blocking(local::restart).await??,
                (Some(t), Some(s)) => deploy::restart(&s, t).await?,
                _ => bail!("not connected"),
            }
            c.info(tr!("Бот перезапущен.", "The bot was restarted.").into());
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_backup(move || {
        cc.task(|c| async move {
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            let data = match (target, session) {
                (Some(Target::Windows), _) => tokio::task::spawn_blocking(local::backup).await??,
                (Some(t), Some(s)) => deploy::backup(&s, t).await?,
                _ => bail!("not connected"),
            };
            let name = format!(
                "hq-watchman-backup-{}.db",
                jiff::Timestamp::now()
                    .to_zoned_datetime(jiff::tz::TimeZone::system())
                    .strftime("%Y%m%d-%H%M")
            );
            let Some(f) = rfd::AsyncFileDialog::new()
                .set_file_name(&name)
                .save_file()
                .await
            else {
                return Ok(());
            };
            tokio::fs::write(f.path(), &data).await?;
            c.info(trf!(
                "Копия сохранена: {}",
                "Backup saved: {}",
                f.path().display()
            ));
            Ok(())
        });
    });

    let cc = c.clone();
    s.on_restore(move || {
        cc.task(|c| async move {
            let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("SQLite", &["db"])
                .pick_file()
                .await
            else {
                return Ok(());
            };
            let data = tokio::fs::read(f.path()).await?;
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            match (target, session) {
                (Some(Target::Windows), _) => {
                    tokio::task::spawn_blocking(move || local::restore(&data)).await??;
                }
                (Some(t), Some(s)) => {
                    let log: Log = Arc::new(|_| {});
                    deploy::restore(&s, t, &data, &log).await?;
                }
                _ => bail!("not connected"),
            }
            c.info(
                tr!(
                    "База восстановлена, бот перезапущен.",
                    "Database restored, the bot was restarted."
                )
                .into(),
            );
            load_managed(&c).await
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_uninstall(move || {
        let Some(ui) = w.upgrade() else { return };
        let keep = ui.global::<S>().get_keep_data();
        let w2 = w.clone();
        cc.task(move |c| async move {
            let (target, session) = {
                let st = lock(&c.st);
                (st.managed.target, st.session.clone())
            };
            match (target, session) {
                (Some(Target::Windows), _) => {
                    tokio::task::spawn_blocking(move || local::uninstall(keep)).await??;
                }
                (Some(t), Some(s)) => {
                    let log: Log = Arc::new(|_| {});
                    deploy::uninstall(&s, t, keep, &log).await?;
                }
                _ => bail!("not connected"),
            }
            let _ = w2.upgrade_in_event_loop(|ui| {
                show(&ui, "welcome");
                ui.global::<S>()
                    .set_info(tr!("Бот удалён.", "The bot was removed.").into());
            });
            Ok(())
        });
    });

    let w = ui.as_weak();
    let cc = c.clone();
    s.on_reconfigure(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = ui.global::<S>();
        let target = lock(&cc.st).managed.target.unwrap_or(Target::Docker);
        s.set_target(target.code().into());
        s.set_server_ok(lock(&cc.st).session.is_some());
        show(&ui, "target");
    });
}

async fn installed_version(c: &Ctx) -> String {
    let (target, session) = {
        let st = lock(&c.st);
        (st.managed.target, st.session.clone())
    };
    match (target, session) {
        (Some(Target::Windows), _) => local::version()
            .split_whitespace()
            .nth(1)
            .unwrap_or("0.0.0")
            .to_owned(),
        (Some(t), Some(s)) => deploy::status(&s, t)
            .await
            .map_or_else(|_| "0.0.0".into(), |st| st.version),
        _ => "0.0.0".into(),
    }
}

fn fill_profiles(s: &S) {
    let rows: Vec<ProfileRow> = store::load()
        .profiles
        .into_iter()
        .map(|p| ProfileRow {
            name: p.name.into(),
            host: p.host.into(),
            port: p.port.to_string().into(),
            user: p.user.into(),
            target: p.target.into(),
            key_path: p.key_path.into(),
        })
        .collect();
    s.set_profiles(ModelRc::new(VecModel::from(rows)));
}

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let ui = MainWindow::new()?;
    let data = store::load();
    let en = match data.language.as_str() {
        "en" => true,
        "ru" => false,
        _ => {
            !std::env::var("LANG")
                .unwrap_or_default()
                .to_lowercase()
                .starts_with("ru")
                && !sys_locale_ru()
        }
    };
    hq_watchman::i18n::set(if en { Lang::En } else { Lang::Ru });
    ui.global::<Tr>().set_en(en);
    let s = ui.global::<S>();
    s.set_version(env!("CARGO_PKG_VERSION").into());
    s.set_windows_host(cfg!(windows));
    s.set_bot_en(en);
    s.set_timezone(
        iana_time_zone::get_timezone()
            .unwrap_or_else(|_| "UTC".into())
            .into(),
    );
    fill_profiles(&s);
    let c = Ctx {
        ui: ui.as_weak(),
        rt: rt.handle().clone(),
        st: Arc::new(Mutex::new(State::default())),
    };
    wire_wizard(&ui, &c);
    wire_manage(&ui, &c);
    refresh_steps(&ui);
    ui.run()?;
    lock(&c.st).cancel.store(true, Ordering::Relaxed);
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(())
}

/// Russian UI by default when the system language is Russian.
fn sys_locale_ru() -> bool {
    sys_locale::get_locale().is_some_and(|l| l.to_lowercase().starts_with("ru"))
}

/// Full run against a real server: `cargo test e2e -- --ignored` with `HQW_E2E_*` variables.
#[cfg(test)]
mod e2e {
    use super::*;

    fn var(n: &str) -> String {
        std::env::var(n).unwrap_or_default()
    }

    async fn run(target: Target) -> Result<()> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let t = ssh::Target {
            host: var("HQW_E2E_HOST"),
            port: var("HQW_E2E_PORT").parse().unwrap_or(22),
            user: var("HQW_E2E_USER"),
            auth: ssh::Auth::Password(Zeroizing::new(var("HQW_E2E_PASSWORD"))),
            sudo_password: None,
        };
        let pinned = store::host_key(&store::load(), &t.host, t.port);
        let session = Session::connect(&t, pinned).await?;
        session.check_root().await?;
        let info = deploy::inspect(&session).await?;
        deploy::supported(&info)?;
        let log: Log = Arc::new(|l| eprintln!("{l}"));
        let bundle = release::Bundle::open(release::folder(var("HQW_E2E_FILES").into())).await?;
        let (profile, _) = services::steam_profile(&var("HQW_E2E_STEAM")).await?;
        let plan = Plan {
            target,
            language: Lang::En,
            github_on: false,
            guides_on: true,
            games_on: true,
            telegram_token: Zeroizing::new(var("HQW_E2E_TOKEN")),
            telegram_admin: var("HQW_E2E_ADMIN").parse().unwrap_or(0),
            github: None,
            steam_profile: Some(profile),
            steam_key: None,
            games: vec![Game::new(572_890, "")],
            timezone: "Europe/Moscow".into(),
            quiet_enabled: false,
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
            proxy: None,
        };
        plan.validate()?;
        match target {
            Target::Docker => deploy::install_docker(&session, &info, &plan, &bundle, &log).await?,
            _ => deploy::install_systemd(&session, &info, &plan, &bundle, &log).await?,
        }
        let info = deploy::inspect(&session).await?;
        let st = deploy::status(&session, target).await?;
        eprintln!("status: {} {}", st.state, st.version);
        let v = deploy::settings_get(&session, target).await?;
        let mut s: Settings = serde_json::from_value(v["settings"].clone())?;
        assert_eq!(s.language, Lang::En);
        s.games.list.push(Game::new(570, "Dota 2"));
        deploy::settings_set(&session, target, &s).await?;
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        let logs = deploy::logs(&session, target, 50).await?;
        assert!(logs.contains("settings changed outside the bot"), "{logs}");
        let db = deploy::backup(&session, target).await?;
        assert!(db.starts_with(b"SQLite format 3"));
        deploy::restore(&session, target, &db, &log).await?;
        deploy::update(&session, &info, target, &bundle, &log).await?;
        deploy::uninstall(&session, target, false, &log).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "needs a test server"]
    async fn e2e_docker() {
        if let Err(e) = run(Target::Docker).await {
            panic!("{e:#}");
        }
    }

    #[tokio::test]
    #[ignore = "needs a test server"]
    async fn e2e_systemd() {
        if let Err(e) = run(Target::Systemd).await {
            panic!("{e:#}");
        }
    }
}
