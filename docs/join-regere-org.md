# Join the regere org from a new machine

A new machine joins the `regere` ikit org in three steps. The new member creates a key, someone
who can already read the org's secrets grants it, and then the new member clones the org.

| Step | Who | What |
| --- | --- | --- |
| 1 | New member | `ikit identity create`, then sends the printed `age1…` public key |
| 2 | Existing key holder (Kaushik) | `ikit org secrets grant age1… --as <member>`, which re-encrypts and pushes |
| 3 | New member | `ikit org clone …` (or `ikit org pull`), then checks with `ikit org secrets list` |

When it's done, the new machine can read `secrets/org.age` and can run commands that need
service secrets, such as moving or deploying services. Today only `kaushiksundar@192.168.1.19`
and the offline `recovery` key can read it.

For sharing and joining orgs in general, see [team.md](team.md).

## Prerequisites (new member)

1. **ikit built from the latest `main`.** The remote state is at schema 15, from the
   `0015_backup_databases` migration. An older build stops with `newer state schema (15 > 14)`.

   ```sh
   cd ~/Documents/claude/regere/ikit
   git pull --ff-only origin main
   cargo build --release -p ikit-cli
   alias ikit="$PWD/target/release/ikit"   # or put target/release on PATH
   ```

2. **An AWS profile that can read and write the state bucket.** The bucket is
   `regere-ikit-backups` in `ap-south-1`, and the state is under `ikit/state/regere/`. Check it with:

   ```sh
   aws s3 ls s3://regere-ikit-backups/ikit/state/regere/ --profile regere --region ap-south-1
   ```

3. **An OS keychain** (GNOME Keyring or KWallet on Linux, Keychain on macOS). ikit keeps the
   private key there.

## Step 1: Create this machine's key (new member)

Run this once per machine. The private key goes into the OS keychain and never leaves it. The
command prints a public key that starts with `age1`.

```sh
ikit identity create
ikit identity show        # prints the same age1… public key again later
```

Send the `age1…` line to Kaushik over Slack or email, along with the label to grant it under
(for example `sridhar.r@regere.ai`). The public key isn't secret.

If `ikit identity show` already prints a key, skip `create`. `identity create --replace` makes a
new key, and every org granted the old one would have to grant the new one.

## Step 2: Grant the key (Kaushik)

Granting re-encrypts `secrets/org.age` to every recipient plus the new key, then pushes a new
generation of the org. Run it on a machine whose key can already read the secrets.

```sh
cd ~/path/to/ikit && git pull --ff-only origin main   # same ikit as the remote (schema 15)
ikit org use regere
ikit org secrets grant age1<paste-the-key> --as sridhar.r@regere.ai
ikit org secrets list     # the new label now appears under org.age
ikit org secrets check    # exits 1 without a recovery key or with fewer than 2 people
```

The regere org has only `org.age` today, with no per-env secrets files. If an environment is
split later (`ikit org secrets split`), also grant that file with `--env <env>`, for example
`--env prod`.

## Step 3: Clone the org and check access (new member)

On a machine with no copy of the org yet:

```sh
ikit org clone s3://regere-ikit-backups/ikit/state/regere --aws-profile regere --region ap-south-1
ikit org use regere
```

If the org is already cloned, pull the grant instead:

```sh
ikit org pull
```

Then check access:

```sh
ikit org secrets list     # org.age should no longer say "you cannot read it"
ikit wl list              # wl-regere-dev-001, -002 and -003
ikit --wl wl-regere-dev-002 svc show decision-hub   # secrets show as •••••• with no error
```

You can clone before the grant. You'll see the org's state but not its secrets, and commands
that need a secret say whose grant is missing.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `… was written by ikit 0.1.0 with a newer state schema (15 > 14); upgrade ikit` | Pull the latest `main` and rebuild (Prerequisites, item 1). |
| `this machine has no identity yet` | Run `ikit identity create` (Step 1). |
| `org.age … you cannot read it` / `your key … cannot read secrets/org.age` | The key isn't granted yet, or the grant hasn't been pulled. Run `ikit org pull`, then ask for Step 2 again if it still fails. |
| `… is locked by <user>@<host> since …` | Someone is writing. Wait, or raise `--lock-timeout`. |
| `the lock held by … expired at …` | A writer crashed. Make sure it isn't running, then `ikit org unlock --force`. |
| `local changes … were never pushed` | Look at `ikit org status`. Redo the change after `ikit org pull --discard`. |
| `profile aws/… keeps its keys in the keychain` | Use an AWS profile from `~/.aws` (`--aws-profile regere`), not ikit's keychain. |
