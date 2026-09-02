---
icon: cloud
---

# Cloud-init

Provisioning, SSH keys, console passwords, static addressing, and what Firestone does with the secrets you give it.

## Cloud-init, keys, and passwords

Firestone's own cloud-init part enables key-only root SSH over vsock, gives the image's default user the same authorized keys, creates the serial getty, grows the root filesystem, onlines hotplugged CPUs, and mounts shared folders. Password SSH stays off unless you turn it on.

Add your own cloud-config from a file:

```sh
cat > user-data.yaml <<'EOF'
#cloud-config
packages:
  - jq
write_files:
  - path: /etc/motd
    permissions: "0644"
    content: "managed by cloud-init\n"
EOF
firestone create configured ubuntu --user-data user-data.yaml
```

Small user-data can live in the machine specification instead. `--user-data` and `--user-data-inline` are mutually exclusive, and identical bytes produce an identical guest either way:

```sh
user_data=$(printf '#cloud-config\npackages: [htop]\n')
firestone create inline ubuntu --user-data-inline "$user_data"
```

Add public keys from files, from the command line, or both:

```sh
firestone create keyed ubuntu --ssh-key "$HOME/.ssh/id_ed25519.pub"
firestone create pasted ubuntu --ssh-authorized-key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P me@laptop"
```

Inline keys are validated exactly like key files, and a key given both ways is written once. Firestone reads its own public key and the public key files you name. It never puts a private key in the seed or in a log.

Set a console login password for `--user` when key-only access is not enough. It is read from a file so it never appears in the process list:

```sh
firestone create console-login ubuntu --password-file ./password.txt
```

The password reaches the guest through cloud-init `chpasswd`, so `firestone console` and a local login accept it immediately. Guest SSH keeps refusing passwords until you also pass `--ssh-pwauth`.

## How secrets are handled

Firestone stores the password as typed, in `machines/<name>/firestone.toml` and in the seed it renders. It is not hashed. Cloud-init's `chpasswd` takes a plaintext value, and a hash Firestone computed would pin one crypt scheme and still be recoverable from the same file. The protection is file permissions, and they are enforced rather than assumed. `firestone.toml`, `seed/meta-data`, `seed/user-data`, `seed/network-config` and `seed.img` are all published mode 0600, inside a mode-0700 directory you own, whatever your umask says.

A password and inline user-data never reach a log line, an event, an error message, a hint or an argument list. `--password-file` is the only spelling of the flag for that reason. Both values stay visible where Firestone is showing your own configuration back to you: `firestone show`, `GET /v1/machines/{name}`, and the `create` result all serialize the effective spec, which is the same data as the file you own. The web interface is bound by the same rule and is not that file. It reports inline user-data as a byte count, authorized keys as a count, and the password as `set` or `unset`, and never renders a submitted password back into the form.

Changing a password, or any effective user-data, keys, network-config, mounts, user, provisioning flag or catalog sshd path, changes the instance id, so the guest re-provisions on the next start.

## Static addressing and no provisioning

Provide NoCloud network-config for a tap guest or anything else that needs static addressing:

```sh
cat > network-config.yaml <<'EOF'
version: 2
ethernets:
  eth0:
    addresses: [192.0.2.10/24]
    routes:
      - to: default
        via: 192.0.2.1
    nameservers:
      addresses: [192.0.2.53]
EOF
firestone create static ubuntu --net tap --tap tap0 --cloud-init-network-config network-config.yaml
```

Relative cloud-init paths resolve from the machine specification's directory after creation. `--user USER` selects the login and console autologin account; the default is `root`, and a different account must already exist in the image or be created by your own cloud-init data with its own keys.

Turn Firestone's provisioning off only when you will provide all guest access yourself:

```sh
firestone create unmanaged ubuntu --no-provisioning --user-data user-data.yaml
```

Without it there is no SSH readiness, no root key injection, no vsock socket unit, no serial autologin and no automatic mounts.

An OCI machine takes no `[cloud_init]` key at all; see [images](images.md). Tap mode setup is in [networking](networking.md). The page list is in the [documentation index](README.md).
