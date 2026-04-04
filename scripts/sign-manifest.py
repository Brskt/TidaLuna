#!/usr/bin/env python3
"""Sign a manifest.json file with Ed25519 using $UPDATE_SIGNING_KEY."""

import base64
import os
import sys

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <manifest.json>", file=sys.stderr)
        sys.exit(1)

    manifest_path = sys.argv[1]
    sig_path = manifest_path + ".sig"

    key_b64 = os.environ.get("UPDATE_SIGNING_KEY")
    if not key_b64:
        print("UPDATE_SIGNING_KEY not set", file=sys.stderr)
        sys.exit(1)

    key_bytes = base64.b64decode(key_b64.strip())
    private_key = Ed25519PrivateKey.from_private_bytes(key_bytes)

    with open(manifest_path, "rb") as f:
        data = f.read()

    signature = private_key.sign(data)

    with open(sig_path, "w") as f:
        f.write(base64.b64encode(signature).decode())

    print(f"Signed {manifest_path} -> {sig_path}")


if __name__ == "__main__":
    main()
