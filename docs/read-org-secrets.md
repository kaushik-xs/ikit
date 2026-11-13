# Read an org's secrets by hand

`ikit` decrypts `secrets/org.age` for you whenever a command needs a secret. Sometimes you want
to look inside it yourself, for example to check a value after a grant or a restore. This page
shows how to do that with `age` and `jq`, using the age identity that `ikit identity create`
stored in the OS keychain.

The commands print secret values to your terminal. Run them only in a private terminal, and
keep the output out of shared screens, logs and chat.

## Where the identity lives

ikit stores the private key under service `ikit`, account `identity/default`:

| OS | Store | Read it with |
| --- | --- | --- |
| macOS | Keychain | `security` (built in) |
| Linux | Secret Service (GNOME Keyring or KWallet) | `secret-tool` (`sudo apt install libsecret-tools`) |

If `IKIT_AGE_IDENTITY` is set (for example during [recovery](team.md#recovery)), ikit uses
that instead of the keychain.

## macOS (bash or zsh)

```sh
age -d -i <(security find-generic-password -s ikit -a identity/default -w) \
  ~/.ikit/orgs/<org>/secrets/org.age | jq .secrets
```

## Linux (bash or zsh)

```sh
age -d -i <(secret-tool lookup service ikit username identity/default) \
  ~/.ikit/orgs/<org>/secrets/org.age | jq .secrets
```

## fish

fish has no `<( … )`. Use `( … | psub)` instead:

```fish
# Linux
age -d -i (secret-tool lookup service ikit username identity/default | psub) ~/.ikit/orgs/<org>/secrets/org.age | jq .secrets

# macOS
age -d -i (security find-generic-password -s ikit -a identity/default -w | psub) ~/.ikit/orgs/<org>/secrets/org.age | jq .secrets
```

Replace `<org>` with the org name, e.g. `regere`. If you set `IKIT_HOME`, use
`$IKIT_HOME/orgs/<org>` instead of `~/.ikit/orgs/<org>`.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| `security: command not found` | You are on Linux. Use the `secret-tool` command above. |
| `fish: Invalid redirection target` | You used `<( … )` in fish. Use `( … \| psub)`. |
| `secret-tool lookup` prints nothing | The keychain entry is missing or stored with other attributes. Run `secret-tool search --all service ikit` to see them (this also prints the values), or run `ikit identity create` if this machine has no key yet. |
| `age: error: no identity matched any of the recipients` | Your key isn't granted on this org yet. See [Join the regere org](join-regere-org.md). |
