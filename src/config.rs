use serde::Deserialize;

/// NoCloud `meta-data` file (YAML).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MetaData {
    #[serde(rename = "instance-id", alias = "instance_id")]
    pub instance_id: Option<String>,
    #[serde(rename = "local-hostname", alias = "local_hostname", alias = "hostname")]
    pub local_hostname: Option<String>,
}

/// Supported subset of `#cloud-config` user-data.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    pub hostname: Option<String>,
    pub fqdn: Option<String>,
    pub manage_etc_hosts: Option<bool>,
    pub users: Vec<UserEntry>,
    pub ssh_authorized_keys: Vec<String>,
    pub write_files: Vec<WriteFile>,
    pub runcmd: Vec<Cmd>,
    pub final_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UserEntry {
    Name(String),
    Spec(UserSpec),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserSpec {
    pub name: String,
    pub gecos: Option<String>,
    pub shell: Option<String>,
    pub homedir: Option<String>,
    pub groups: Option<Groups>,
    pub sudo: Option<SudoVal>,
    /// Pre-hashed password (crypt(3) format), applied with `chpasswd -e`.
    pub passwd: Option<String>,
    pub lock_passwd: Option<bool>,
    pub system: Option<bool>,
    pub ssh_authorized_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Groups {
    Str(String),
    List(Vec<String>),
}

impl Groups {
    pub fn joined(&self) -> String {
        match self {
            Groups::Str(s) => s.split([',', ' ']).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(","),
            Groups::List(v) => v.join(","),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SudoVal {
    // `sudo: false` deserializes here; the value itself is never consulted.
    Enabled(#[allow(dead_code)] bool),
    Line(String),
    Lines(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WriteFile {
    pub path: String,
    pub content: String,
    pub encoding: Option<String>,
    pub permissions: Option<PermVal>,
    /// "user" or "user:group"
    pub owner: Option<String>,
    pub append: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PermVal {
    Num(u32),
    Str(String),
}

/// Resolve a `permissions:` value to a mode. Quoted strings ("0644") are
/// parsed as octal; a bare YAML integer with a leading zero (0644) is already
/// octal per YAML 1.1 and is used as-is.
pub fn parse_mode(p: Option<&PermVal>) -> Result<u32, String> {
    match p {
        None => Ok(0o644),
        Some(PermVal::Num(n)) => Ok(*n),
        Some(PermVal::Str(s)) => {
            let t = s.trim();
            let t = t.strip_prefix("0o").unwrap_or(t);
            u32::from_str_radix(t, 8).map_err(|e| format!("invalid permissions '{s}': {e}"))
        }
    }
}

pub fn decode_content(content: &str, encoding: Option<&str>) -> Result<Vec<u8>, String> {
    match encoding.unwrap_or("plain") {
        "" | "plain" | "text/plain" => Ok(content.as_bytes().to_vec()),
        "b64" | "base64" => {
            use base64::Engine;
            let cleaned: String = content.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(cleaned.as_bytes())
                .map_err(|e| format!("invalid base64 content: {e}"))
        }
        other => Err(format!("unsupported write_files encoding '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meta_data() {
        let m: MetaData = serde_yaml::from_str("instance-id: iid-01\nlocal-hostname: node1\n").unwrap();
        assert_eq!(m.instance_id.as_deref(), Some("iid-01"));
        assert_eq!(m.local_hostname.as_deref(), Some("node1"));
    }

    #[test]
    fn parse_full_cloud_config() {
        let yaml = r#"#cloud-config
hostname: node1
fqdn: node1.g8.lo
manage_etc_hosts: true
users:
  - default
  - name: glenn
    gecos: Glenn
    shell: /bin/bash
    groups: wheel,adm
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    lock_passwd: false
    passwd: "$6$abc$hash"
    ssh_authorized_keys:
      - ssh-ed25519 AAAA key1
ssh_authorized_keys:
  - ssh-ed25519 BBBB rootkey
write_files:
  - path: /etc/motd
    content: hello
    permissions: '0600'
  - path: /etc/blob
    content: aGVsbG8=
    encoding: b64
    permissions: 0644
    append: true
runcmd:
  - echo one
  - [systemctl, enable, chronyd]
final_message: done
"#;
        let c: CloudConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(c.hostname.as_deref(), Some("node1"));
        assert_eq!(c.users.len(), 2);
        match &c.users[0] {
            UserEntry::Name(n) => assert_eq!(n, "default"),
            _ => panic!("expected name entry"),
        }
        match &c.users[1] {
            UserEntry::Spec(u) => {
                assert_eq!(u.name, "glenn");
                assert_eq!(u.groups.as_ref().unwrap().joined(), "wheel,adm");
                assert!(matches!(u.sudo, Some(SudoVal::Line(_))));
                assert_eq!(u.ssh_authorized_keys.len(), 1);
            }
            _ => panic!("expected spec entry"),
        }
        assert_eq!(c.write_files.len(), 2);
        assert_eq!(parse_mode(c.write_files[0].permissions.as_ref()).unwrap(), 0o600);
        // bare 0644 is YAML octal
        assert_eq!(parse_mode(c.write_files[1].permissions.as_ref()).unwrap(), 0o644);
        assert!(c.write_files[1].append);
        assert_eq!(c.runcmd.len(), 2);
        assert!(matches!(c.runcmd[0], Cmd::Shell(_)));
        assert!(matches!(c.runcmd[1], Cmd::Argv(_)));
    }

    #[test]
    fn groups_list_form() {
        let g: Groups = serde_yaml::from_str("[wheel, adm]").unwrap();
        assert_eq!(g.joined(), "wheel,adm");
    }

    #[test]
    fn decode_plain_and_b64() {
        assert_eq!(decode_content("hi", None).unwrap(), b"hi");
        assert_eq!(decode_content("aGVs\nbG8=", Some("b64")).unwrap(), b"hello");
        assert!(decode_content("x", Some("gzip")).is_err());
        assert!(decode_content("!!!", Some("b64")).is_err());
    }

    #[test]
    fn parse_mode_variants() {
        assert_eq!(parse_mode(None).unwrap(), 0o644);
        assert_eq!(parse_mode(Some(&PermVal::Str("0o755".into()))).unwrap(), 0o755);
        assert!(parse_mode(Some(&PermVal::Str("9z".into()))).is_err());
    }
}
