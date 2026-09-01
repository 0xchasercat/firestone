//! Resolving the image's `user` value against the guest's own account files.
//!
//! SPEC §10.5: `user` is resolved "through the image's own `/etc/passwd` when it
//! is not `root`". An OCI image may also name a bare numeric id, or a
//! `user:group` pair, so this module accepts both spellings and falls back to
//! numeric parsing. It is pure text handling: the caller supplies the file
//! contents, which is what makes it testable off a Linux host.

use std::fmt;

/// The account the entrypoint runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUser {
    pub uid: u32,
    pub gid: u32,
    pub home: Option<String>,
}

impl ResolvedUser {
    /// The account used when the image names none.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            uid: 0,
            gid: 0,
            home: None,
        }
    }
}

/// Why an image `user` value could not be resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum UserError {
    /// The value was empty or contained more than one `:`.
    Malformed { value: String },
    /// A user name that is not in `/etc/passwd` and is not a number.
    UnknownUser { name: String },
    /// A group name that is not in `/etc/group` and is not a number.
    UnknownGroup { name: String },
}

impl fmt::Display for UserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { value } => {
                write!(formatter, "image user '{value}' is not 'user[:group]'")
            }
            Self::UnknownUser { name } => write!(
                formatter,
                "image user '{name}' is not in /etc/passwd and is not a numeric uid"
            ),
            Self::UnknownGroup { name } => write!(
                formatter,
                "image group '{name}' is not in /etc/group and is not a numeric gid"
            ),
        }
    }
}

impl std::error::Error for UserError {}

/// One parsed `/etc/passwd` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: Option<String>,
}

/// Resolves `user[:group]` against `/etc/passwd` and `/etc/group` contents.
///
/// An empty or `root` value resolves to uid 0 / gid 0 without requiring either
/// file, so an image with no account database still boots.
pub fn resolve_user(value: &str, passwd: &str, group: &str) -> Result<ResolvedUser, UserError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(ResolvedUser::root());
    }
    let (user_part, group_part) = match trimmed.split_once(':') {
        Some((user, rest)) => {
            if rest.contains(':') || user.is_empty() || rest.is_empty() {
                return Err(UserError::Malformed {
                    value: trimmed.to_owned(),
                });
            }
            (user, Some(rest))
        }
        None => (trimmed, None),
    };

    let entries = parse_passwd(passwd);
    let mut resolved = resolve_user_part(user_part, &entries)?;
    if let Some(group_part) = group_part {
        resolved.gid = resolve_group_part(group_part, group)?;
    }
    Ok(resolved)
}

fn resolve_user_part(value: &str, entries: &[PasswdEntry]) -> Result<ResolvedUser, UserError> {
    if let Some(entry) = entries.iter().find(|entry| entry.name == value) {
        return Ok(ResolvedUser {
            uid: entry.uid,
            gid: entry.gid,
            home: entry.home.clone(),
        });
    }
    if let Ok(uid) = value.parse::<u32>() {
        // A numeric id that the account database also knows adopts that row's
        // primary group and home; an unknown id runs with gid == uid, which is
        // what a container runtime does with `--user 1000` on an image whose
        // passwd has no such row.
        return Ok(entries.iter().find(|entry| entry.uid == uid).map_or_else(
            || ResolvedUser {
                uid,
                gid: uid,
                home: None,
            },
            |entry| ResolvedUser {
                uid: entry.uid,
                gid: entry.gid,
                home: entry.home.clone(),
            },
        ));
    }
    if value == "root" {
        return Ok(ResolvedUser::root());
    }
    Err(UserError::UnknownUser {
        name: value.to_owned(),
    })
}

fn resolve_group_part(value: &str, group: &str) -> Result<u32, UserError> {
    if let Ok(gid) = value.parse::<u32>() {
        return Ok(gid);
    }
    for line in significant_lines(group) {
        let mut fields = line.split(':');
        let (Some(name), Some(_), Some(gid)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if name == value {
            if let Ok(gid) = gid.parse::<u32>() {
                return Ok(gid);
            }
        }
    }
    if value == "root" {
        return Ok(0);
    }
    Err(UserError::UnknownGroup {
        name: value.to_owned(),
    })
}

fn parse_passwd(passwd: &str) -> Vec<PasswdEntry> {
    let mut entries = Vec::new();
    for line in significant_lines(passwd) {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() < 6 {
            continue;
        }
        let (Ok(uid), Ok(gid)) = (fields[2].parse::<u32>(), fields[3].parse::<u32>()) else {
            continue;
        };
        entries.push(PasswdEntry {
            name: fields[0].to_owned(),
            uid,
            gid,
            home: (!fields[5].is_empty()).then(|| fields[5].to_owned()),
        });
    }
    entries
}

fn significant_lines(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

#[cfg(test)]
mod tests {
    use super::{ResolvedUser, UserError, resolve_user};

    const PASSWD: &str = "\
# comment
root:x:0:0:root:/root:/bin/sh
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
nginx:x:101:102::/var/cache/nginx:/sbin/nologin
broken:x:notanumber:0::/tmp:/bin/sh
short:x:5
";

    const GROUP: &str = "\
root:x:0:
nginx:x:102:
malformed
";

    #[test]
    fn resolve_user_named_account_uses_its_passwd_row() -> Result<(), UserError> {
        assert_eq!(
            resolve_user("nginx", PASSWD, GROUP)?,
            ResolvedUser {
                uid: 101,
                gid: 102,
                home: Some("/var/cache/nginx".to_owned()),
            }
        );
        Ok(())
    }

    #[test]
    fn resolve_user_empty_value_is_root() -> Result<(), UserError> {
        assert_eq!(resolve_user("   ", PASSWD, GROUP)?, ResolvedUser::root());
        Ok(())
    }

    #[test]
    fn resolve_user_root_without_account_files_is_uid_zero() -> Result<(), UserError> {
        assert_eq!(resolve_user("root", "", "")?, ResolvedUser::root());
        Ok(())
    }

    #[test]
    fn resolve_user_known_numeric_uid_adopts_its_row() -> Result<(), UserError> {
        assert_eq!(
            resolve_user("101", PASSWD, GROUP)?,
            ResolvedUser {
                uid: 101,
                gid: 102,
                home: Some("/var/cache/nginx".to_owned()),
            }
        );
        Ok(())
    }

    #[test]
    fn resolve_user_unknown_numeric_uid_uses_the_same_gid() -> Result<(), UserError> {
        assert_eq!(
            resolve_user("4242", PASSWD, GROUP)?,
            ResolvedUser {
                uid: 4242,
                gid: 4242,
                home: None,
            }
        );
        Ok(())
    }

    #[test]
    fn resolve_user_named_group_overrides_the_primary_group() -> Result<(), UserError> {
        assert_eq!(
            resolve_user("daemon:nginx", PASSWD, GROUP)?,
            ResolvedUser {
                uid: 1,
                gid: 102,
                home: Some("/usr/sbin".to_owned()),
            }
        );
        Ok(())
    }

    #[test]
    fn resolve_user_numeric_pair_needs_no_account_files() -> Result<(), UserError> {
        assert_eq!(
            resolve_user("1000:2000", "", "")?,
            ResolvedUser {
                uid: 1000,
                gid: 2000,
                home: None,
            }
        );
        Ok(())
    }

    #[test]
    fn resolve_user_unknown_name_is_refused() {
        assert_eq!(
            resolve_user("nobody", PASSWD, GROUP),
            Err(UserError::UnknownUser {
                name: "nobody".to_owned()
            })
        );
    }

    #[test]
    fn resolve_user_unknown_group_is_refused() {
        assert_eq!(
            resolve_user("root:wheel", PASSWD, GROUP),
            Err(UserError::UnknownGroup {
                name: "wheel".to_owned()
            })
        );
    }

    #[test]
    fn resolve_user_extra_colon_is_malformed() {
        assert_eq!(
            resolve_user("a:b:c", PASSWD, GROUP),
            Err(UserError::Malformed {
                value: "a:b:c".to_owned()
            })
        );
    }

    #[test]
    fn resolve_user_ignores_malformed_passwd_rows() -> Result<(), UserError> {
        // `broken` has a non-numeric uid and `short` has too few fields; both
        // are skipped rather than aborting the lookup.
        assert_eq!(
            resolve_user("daemon", PASSWD, GROUP)?,
            ResolvedUser {
                uid: 1,
                gid: 1,
                home: Some("/usr/sbin".to_owned()),
            }
        );
        Ok(())
    }
}
