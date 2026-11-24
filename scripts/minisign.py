# SPDX-License-Identifier: GPL-3.0-or-later
# Minisign keys without a password and prehashed (ED) signatures.
# Usage: minisign.py generate <secret> <public> | sign <secret> <file> <trusted comment>
import base64
import hashlib
import os
import sys

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat


def b2(data, size):
    return hashlib.blake2b(data, digest_size=size).digest()


def generate(secret_path, public_path):
    key = Ed25519PrivateKey.generate()
    seed = key.private_bytes_raw()
    pk = key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    key_id = os.urandom(8)
    sk = seed + pk
    checksum = b2(b"Ed" + key_id + sk, 32)
    blob = b"Ed" + b"\0\0" + b"B2" + bytes(32) + bytes(8) + bytes(8) + key_id + sk + checksum
    hex_id = key_id[::-1].hex().upper()
    fd = os.open(secret_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write("untrusted comment: minisign secret key " + hex_id + "\n")
        f.write(base64.b64encode(blob).decode() + "\n")
    with open(public_path, "w") as f:
        f.write("untrusted comment: minisign public key " + hex_id + "\n")
        f.write(base64.b64encode(b"Ed" + key_id + pk).decode() + "\n")


def sign(secret_path, file_path, comment):
    lines = open(secret_path).read().split("\n")
    blob = base64.b64decode(lines[1])
    if blob[:2] != b"Ed" or blob[2:4] != b"\0\0":
        sys.exit("unsupported secret key")
    key_id = blob[54:62]
    sk = blob[62:126]
    if b2(b"Ed" + key_id + sk, 32) != blob[126:158]:
        sys.exit("secret key checksum mismatch")
    key = Ed25519PrivateKey.from_private_bytes(sk[:32])
    data = open(file_path, "rb").read()
    sig = key.sign(b2(data, 64))
    global_sig = key.sign(sig + comment.encode())
    with open(file_path + ".minisig", "w") as f:
        f.write("untrusted comment: signature from minisign secret key\n")
        f.write(base64.b64encode(b"ED" + key_id + sig).decode() + "\n")
        f.write("trusted comment: " + comment + "\n")
        f.write(base64.b64encode(global_sig).decode() + "\n")


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "generate":
        generate(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 5 and sys.argv[1] == "sign":
        sign(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        sys.exit(__doc__ or "usage: minisign.py generate <secret> <public> | sign <secret> <file> <comment>")
