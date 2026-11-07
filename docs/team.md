# Running an org as a team

One ikit org can be shared by several SREs. Its state lives in object storage as numbered
snapshots, and its secrets live in age-encrypted files that travel with that state. Each person
needs two things: their own cloud credentials for the state's bucket, and a public key that
someone has granted. Removing either one stops that person.

This guide uses S3 and an org called `acme`. Azure Blob, GCS and a local or NAS folder work the
same way (see `ikit org clone --help`).

## Install

```sh
cargo install --git https://github.com/kaushik-xs/ikit ikit-cli --locked
ikit --version
```

Everyone on the team should run the same ikit version. A snapshot written by a newer ikit is
refused by an older one with a message naming the version to install.

## Share an org (once, by its owner)

```sh
# 1. Your key. The private half stays in the OS keychain.
ikit identity create

# 2. A break-glass recovery key. Store the private half offline (a password manager, or
#    printed in a safe), then delete any copy on disk.
ikit identity create --print-only

# 3. A storage whose credentials come from ~/.aws, not from ikit's keychain.
ikit profile add aws team --set region=eu-central-1 --set aws_profile=<your ~/.aws profile>
ikit storage add s3 --provider s3 --bucket <bucket> --prefix ikit --profile team

# 4. Push the org. This also moves its keychain secrets into secrets/org.age.
ikit org remote set --storage s3
ikit org secrets grant <recovery public key> --as recovery

# 5. Optional: a smaller group for prod secrets.
ikit org secrets split prod
```

Turn on versioning for the bucket, so even pruned or overwritten snapshots can be recovered:

```sh
aws s3api put-bucket-versioning --bucket <bucket> --versioning-configuration Status=Enabled
```

## Give an SRE access

Each SRE needs read and write access to the org's prefix. ikit deletes objects too: the lock
on release, and old snapshots when pruning. This policy covers the org's state and its backups
under the `ikit/` prefix:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::<bucket>",
      "Condition": { "StringLike": { "s3:prefix": ["ikit/*"] } }
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::<bucket>/ikit/*"
    }
  ]
}
```

To keep someone out of the backups, narrow both prefixes to `ikit/state/<org>/`.

## Join an org (each new SRE)

1. Make your key and send the printed `age1…` line to someone already on the org. A public key
   is not secret.

   ```sh
   ikit identity create
   ```

2. They grant it, which re-encrypts the secrets and pushes:

   ```sh
   ikit org secrets grant age1… --as alice@example.com
   ikit org secrets grant age1… --as alice@example.com --env prod   # if prod has its own file
   ```

3. Clone the org with your own AWS profile:

   ```sh
   ikit org clone s3://<bucket>/ikit/state/acme --aws-profile <yours> --region eu-central-1
   ikit org use acme
   ikit org secrets list        # shows the files your key can read
   ```

You can clone before you are granted. You then see the org's state but not its secrets, and
commands that need a secret say whose grant you are missing.

## Day to day

Nothing changes in how you run ikit. Every command catches up with the latest generation first.
Commands that change state take the org's lock and push a new generation when they finish.

| Command | Use |
| --- | --- |
| `ikit org status` | Your generation vs the remote, unpushed changes, who holds the lock |
| `ikit org history` | Who changed what, newest first |
| `ikit org restore <gen>` | Make an old generation current again (as a new one) |
| `ikit org pull --discard` | Drop local changes that never reached the remote |
| `ikit org push` | Push what a failed command left behind |
| `ikit org unlock --force` | Clear the lock of a writer that crashed (after checking it is not running) |
| `ikit org secrets list` | Who can read each secrets file |

When someone else holds the lock, a command waits up to 60 seconds (`--lock-timeout`) and then
fails, naming the holder and their command. Long deploys renew the lock as they run. A crashed
writer's lock lapses within 10 minutes.

When a pull changes who can read the secrets, ikit says so, naming who gained or lost access.
Treat an unexpected change as an incident: anyone with write access to the bucket can edit the
recipients file.

## Buckets, queues and their keys

Resources ([resources.md](resources.md)) are created with a backend's provisioning credentials,
which come from an `aws` profile naming a `~/.aws` profile (`--set aws_profile=ikit-acme`). Never
share those keys: give each provisioner an IAM user of their own in one group that carries the
provisioning policy (`ikit res backend iam-policy <backend>` prints it), and the same profile
name on every machine, so the shared org works everywhere while each machine acts as its own
user.

```sh
aws iam create-user --user-name ikit-acme-alice
aws iam add-user-to-group --group-name ikit-acme-provisioners --user-name ikit-acme-alice
aws iam create-access-key --user-name ikit-acme-alice   # alice puts it in ~/.aws as [ikit-acme]
```

On backends with per-service keys, only creating or changing something needs those credentials:
a new bucket, queue or service, a policy change, `res rotate`. Plan and apply of a workload whose
resources and service users exist already need none, so SREs who only deploy don't need to be
provisioners. The services' own keys travel in the org's secrets. On backends without per-service
keys, services get the provisioning keys, so everyone who deploys needs them.

## Offboard an SRE

```sh
ikit org secrets revoke alice@example.com                 # and --env prod, if granted
```

Then remove their bucket access and their provisioning IAM user, and rotate every secret they
could read (`ikit res rotate <service>` for services' own keys). Revoking stops them
reading new versions of the secrets, not copies they already have, and older snapshots still
open with their key.

## CI and automation

Give each pipeline its own key, labelled `ci-…` so `secrets check` does not count it as a
person:

```sh
ikit identity create --print-only                         # private half -> CI secret store
ikit org secrets grant age1… --as ci-deploy --env prod
```

In the pipeline, set `IKIT_AGE_IDENTITY` to the private key and give the job AWS credentials for
the bucket. CI takes the same lock as people do.

Add a guard to CI so the org cannot drift into a state where one lost laptop locks everyone
out:

```sh
ikit org secrets check          # exits 1 without a recovery key or with fewer than 2 people
```

## Recovery

If every person's key is lost:

1. On a trusted machine, set `IKIT_AGE_IDENTITY` to the recovery key's private half.
2. `ikit org clone s3://<bucket>/ikit/state/acme --aws-profile <yours> --region eu-central-1`
3. `ikit identity create` for the new owner and `ikit org secrets grant <their key> --as <them>`.
4. Revoke the recovery key, make a new one with `ikit identity create --print-only`, grant it as
   `recovery`, and store it offline. The old one was exposed while you used it.

## Troubleshooting

| Message | Meaning |
| --- | --- |
| `… is locked by bob@host since …` | Someone is writing. Wait, or raise `--lock-timeout`. |
| `the lock held by … expired at …` | A writer crashed. Check it is gone, then `ikit org unlock --force`. |
| `local changes … were never pushed, and … is now at generation N` | A command failed after changing your copy while someone else pushed. Look at `ikit org status`, redo the change after `ikit org pull --discard`. |
| `your key … cannot read secrets/org.age` | Ask someone on the org to grant your key. |
| `… was written by ikit X with a newer state schema` | Upgrade ikit. |
| `profile aws/… keeps its keys in the keychain` | The remote must be reachable without the org's secrets: use an `aws` profile with `aws_profile`. |
