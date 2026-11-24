# The release signing key

`acpilot update` replaces the program it is running only when the release says who
published it. A `.sha256` file proves a download arrived intact. The `.sig` beside it
proves the release published that digest, and only a key that never travels with the
release can say so (ADR 0011 D4).

Two pieces of configuration, both on the repository rather than in the tree:

| Where | Name | What it holds |
| --- | --- | --- |
| Repository secret | `ACPILOT_SIGNING_KEY` | the private key, PKCS#8 PEM. `release.yml` signs every checksum with it |
| Repository variable | `ACPILOT_RELEASE_PUBKEY` | the public key, 64 hex characters. Compiled into `acpilot` by the archives job |
| Repository variable, optional | `ACPILOT_KEY_ROTATE_BY` | the date this key should retire, `YYYY-MM-DD`. The weekly credential check warns as it nears |

The weekly `release credentials` job warns a month before the tap token ends, and a month before `ACPILOT_KEY_ROTATE_BY` passes. The key itself has no expiry date.

A release with the secret missing fails in its signing step. A binary built without the
variable refuses to update itself, and says so, rather than trusting a checksum alone.

## Making the key

```bash
openssl genpkey -algorithm ed25519 -out acpilot-release.pem
openssl pkey -in acpilot-release.pem -pubout -out acpilot-release.pub.pem
```

The variable is the public key's raw 32 bytes, which is the tail of the PEM's DER body:

```bash
openssl pkey -in acpilot-release.pem -pubout -outform DER \
  | tail -c 32 | od -An -tx1 -v | tr -d ' \n'
echo
```

Set the secret to the PEM file's contents, including the `-----BEGIN PRIVATE KEY-----`
line. Set the variable to the hex above. Then check that the pair agrees before you
trust it. Build with `ACPILOT_RELEASE_PUBKEY` set to the hex. Sign a file with the
PEM. `acpilot update` will accept that signature and refuse any other.

`openssl pkeyutl -sign -rawin -inkey acpilot-release.pem -in <file> -out <file>.sig`
produces exactly the 64-byte signature the updater checks, which is what `release.yml`
runs for each `*.sha256` it downloads.

## Rotating it

The key is compiled into the binary, so a rotation is two releases and a window:

1. Release a version whose `ACPILOT_RELEASE_PUBKEY` is the **new** key. It verifies the
   new key's signatures, and everyone on an older build still verifies the old one.
2. Once that version is the oldest anyone is likely to run, sign with the new key and
   let the old one go.

A single-key updater cannot check two keys at once, which is the cost of not having a
keyring. The window above is what keeps the transition from stranding a machine that
skipped a release.

## What is not signed

The installers and the formula are not signed, and `--from` is not checked at all. The
installer's own trust is the transport that delivered it (`curl | sh` over TLS, or a
tap). `--from` is a person carrying a binary to a machine that cannot download, and
that person is the one deciding. Neither claims to be a release.
