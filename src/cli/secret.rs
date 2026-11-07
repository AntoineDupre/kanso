//! CLI-side capabilities. Values are never written to files or printed here.
use super::{
    client::{Client, Method},
    output, CliError, SecretCommand,
};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn private_dir() -> Result<PathBuf, CliError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CliError::usage("HOME is required for secret capability storage"))?;
    if !Path::new(&home).is_absolute() {
        return Err(CliError::usage("HOME must be an absolute path"));
    }
    let path = PathBuf::from(home).join(".local/state/kanso/secret-handoffs");
    ensure_private_dir(&path)?;
    let canonical = fs::canonicalize(&path)
        .map_err(|_| CliError::Io("cannot inspect private capability directory".into()))?;
    let mut current = std::env::current_dir()
        .map_err(|_| CliError::Io("cannot inspect current directory".into()))?;
    loop {
        if current.join(".git").exists() && canonical.starts_with(&current) {
            return Err(CliError::Io(
                "capability directory must be outside the repository".into(),
            ));
        }
        if !current.pop() {
            break;
        }
    }
    Ok(path)
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut part = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => part.push(component.as_os_str()),
            _ => return Err(CliError::Io("invalid private directory path".into())),
        }
        if matches!(component, Component::RootDir) {
            continue;
        }
        match fs::symlink_metadata(&part) {
            Ok(meta) if meta.file_type().is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(CliError::Io(
                    "symlink or non-directory in capability path".into(),
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&part).map_err(|_| {
                    CliError::Io("cannot create private capability directory".into())
                })?;
            }
            Err(_) => return Err(CliError::Io("cannot inspect capability directory".into())),
        }
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|_| CliError::Io("cannot inspect capability directory".into()))?;
    if meta.permissions().mode() & 0o777 != 0o700 {
        return Err(CliError::Io(
            "capability directory must have mode 0700".into(),
        ));
    }
    Ok(())
}
#[cfg(not(unix))]
fn ensure_private_dir(_: &Path) -> Result<(), CliError> {
    Err(CliError::Io(
        "private capability storage requires Unix permissions".into(),
    ))
}

fn path(dir: &Path, id: &str) -> Result<PathBuf, CliError> {
    if !valid_id(id) {
        return Err(CliError::usage("invalid request ID"));
    }
    Ok(dir.join(id))
}
#[cfg(unix)]
fn save(dir: &Path, id: &str, capability: &str) -> Result<(), CliError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = path(dir, id)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|_| CliError::Io("cannot create private capability file".into()))?;
    if file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|_| file.write_all(capability.as_bytes()))
        .and_then(|_| file.sync_all())
        .is_err()
    {
        let _ = fs::remove_file(path);
        return Err(CliError::Io("cannot save capability".into()));
    }
    Ok(())
}
#[cfg(not(unix))]
fn save(_: &Path, _: &str, _: &str) -> Result<(), CliError> {
    Err(CliError::Io("unsupported platform".into()))
}

#[cfg(unix)]
fn load(dir: &Path, id: &str) -> Result<String, CliError> {
    use std::os::unix::fs::PermissionsExt;
    let path = path(dir, id)?;
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| CliError::Io("capability file unavailable".into()))?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.permissions().mode() & 0o777 != 0o600
    {
        return Err(CliError::Io(
            "capability file must be a private regular file".into(),
        ));
    }
    let mut value = String::new();
    fs::File::open(path)
        .and_then(|f| f.take(256).read_to_string(&mut value))
        .map_err(|_| CliError::Io("cannot read capability".into()))?;
    if value.len() != 43
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(CliError::Io("invalid capability file".into()));
    }
    Ok(value)
}
#[cfg(not(unix))]
fn load(_: &Path, _: &str) -> Result<String, CliError> {
    Err(CliError::Io("unsupported platform".into()))
}

fn remove_required(dir: &Path, id: &str) -> Result<(), CliError> {
    match fs::remove_file(path(dir, id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CliError::Io(
            "could not remove local capability file".into(),
        )),
    }
}
fn safe_response(response: super::client::ApiResponse) -> Result<String, CliError> {
    if (200..300).contains(&response.status) {
        Ok(response.body)
    } else {
        Err(CliError::Api {
            status: response.status,
            code: "secret_request".into(),
            message: "secret operation failed; inspect status or request again".into(),
        })
    }
}
fn parsed(body: &str) -> Result<Value, CliError> {
    serde_json::from_str(body).map_err(|_| CliError::Io("invalid secret response".into()))
}
fn send(
    client: &Client,
    method: Method,
    url: &str,
    body: Option<&Value>,
) -> Result<super::client::ApiResponse, CliError> {
    client
        .request(method, url, &[], body, None)
        .map_err(|_| CliError::Network("secret operation failed; outcome may be unknown".into()))
}
fn env_name(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn run(command: SecretCommand, json_output: bool) -> Result<(), CliError> {
    let client = Client::secret_from_config()?;
    match command {
        SecretCommand::Request {
            reference,
            label,
            purpose,
        } => {
            let dir = private_dir()?;
            // Card references contain only project identifiers, not arbitrary URL paths.
            if !reference
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                return Err(CliError::usage("invalid card reference"));
            }
            let id = crate::ids::uuid_v4()
                .map_err(|_| CliError::Io("random generator failed".into()))?;
            let capability = crate::ids::base64_url(
                &crate::ids::random_bytes::<32>()
                    .map_err(|_| CliError::Io("random generator failed".into()))?,
            );
            save(&dir, &id, &capability)?;
            let uncertain = || {
                CliError::Io(format!("creation outcome unknown for request {id}; inspect with 'kanso secret status {id}' or cancel with 'kanso secret cancel {id}'; do not retry creation automatically"))
            };
            let response = send(
                &client,
                Method::Post,
                &format!("/api/tasks/{reference}/secrets"),
                Some(&json!({"label":label,"purpose":purpose,"id":id,"capability":capability})),
            )
            .map_err(|_| uncertain())?;
            if (400..500).contains(&response.status) {
                remove_required(&dir, &id)?;
                return Err(CliError::Api {
                    status: response.status,
                    code: "secret_request".into(),
                    message: "secret request rejected".into(),
                });
            }
            if !(200..300).contains(&response.status) {
                return Err(uncertain());
            }
            let data = parsed(&response.body).map_err(|_| uncertain())?;
            if data["id"].as_str() != Some(id.as_str())
                || data["capability"].as_str() != Some(capability.as_str())
            {
                return Err(uncertain());
            }
            if json_output {
                output::line(format_args!("{}", json!({"id":id})));
            } else {
                output::line(format_args!("{id}"));
            }
            Ok(())
        }
        SecretCommand::Status { request } => {
            if !valid_id(&request) {
                return Err(CliError::usage("invalid request ID"));
            }
            let data = parsed(&safe_response(send(
                &client,
                Method::Get,
                &format!("/api/secrets/{request}"),
                None,
            )?)?)?;
            if matches!(
                data["state"].as_str(),
                Some("consumed" | "declined" | "cancelled" | "expired" | "invalidated")
            ) {
                remove_required(&private_dir()?, &request)?;
            }
            let safe = json!({"id":data["id"],"task_id":data["task_id"],"label":data["label"],"purpose":data["purpose"],"state":data["state"],"deadline":data["deadline"],"value_deadline":data["value_deadline"],"delivery_status":data["delivery_status"],"invalidation_reason":data["invalidation_reason"]});
            output::line(format_args!("{}", safe));
            Ok(())
        }
        SecretCommand::Cancel { request } => {
            if !valid_id(&request) {
                return Err(CliError::usage("invalid request ID"));
            }
            let response = send(
                &client,
                Method::Post,
                &format!("/api/secrets/{request}/cancel"),
                None,
            )?;
            if response.status == 409 {
                if let Ok(status) = send(
                    &client,
                    Method::Get,
                    &format!("/api/secrets/{request}"),
                    None,
                )
                .and_then(safe_response)
                .and_then(|text| parsed(&text))
                {
                    if matches!(
                        status["state"].as_str(),
                        Some("consumed" | "declined" | "cancelled" | "expired" | "invalidated")
                    ) {
                        remove_required(&private_dir()?, &request)?;
                        output::line(format_args!("{}", json!({"status":status["state"]})));
                        return Ok(());
                    }
                }
            }
            safe_response(response)?;
            remove_required(&private_dir()?, &request)?;
            output::line(format_args!(
                "{}",
                if json_output {
                    "{\"status\":\"cancelled\"}"
                } else {
                    "cancelled"
                }
            ));
            Ok(())
        }
        SecretCommand::Exec {
            request,
            env,
            command,
        } => {
            if !env_name(&env) {
                return Err(CliError::usage("invalid environment variable name"));
            }
            if command.is_empty() {
                return Err(CliError::usage("a child command is required after --"));
            }
            let dir = private_dir()?;
            let capability = load(&dir, &request)?;
            let result = send(
                &client,
                Method::Post,
                &format!("/api/secrets/{request}/consume"),
                Some(&json!({"capability":capability})),
            );
            // An uncertain response may have consumed the value: never reuse the capability.
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    remove_required(&dir, &request)?;
                    return Err(error);
                }
            };
            if (200..300).contains(&response.status) || response.status >= 500 {
                remove_required(&dir, &request)?;
            } else if response.status == 409 {
                // Status is metadata-only; this is not a retry of consumption.
                if let Ok(status) = send(
                    &client,
                    Method::Get,
                    &format!("/api/secrets/{request}"),
                    None,
                )
                .and_then(safe_response)
                .and_then(|text| parsed(&text))
                {
                    if matches!(
                        status["state"].as_str(),
                        Some("consumed" | "declined" | "cancelled" | "expired" | "invalidated")
                    ) {
                        remove_required(&dir, &request)?;
                    }
                }
            }
            let body = safe_response(response)?;
            let data = parsed(&body)?;
            let value = data["value"]
                .as_str()
                .ok_or_else(|| CliError::Io("invalid consume response; request again".into()))?;
            let status = Command::new(&command[0])
                .args(&command[1..])
                .env(env, value)
                .status()
                .map_err(|_| CliError::Io("could not launch child; request again".into()))?;
            if status.success() {
                return Ok(());
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                Err(CliError::Child(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
                        .clamp(1, 255) as u8,
                ))
            }
            #[cfg(not(unix))]
            {
                Err(CliError::Child(
                    status.code().unwrap_or(1).clamp(1, 255) as u8
                ))
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    const ID: &str = "12345678-1234-1234-1234-123456789abc";

    #[test]
    fn private_files_refuse_symlinks_and_overwrite() {
        let temp = crate::test_support::TempDir::new();
        let dir = temp.path().join("private");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let cap = "a".repeat(43);
        save(&dir, ID, &cap).unwrap();
        assert_eq!(
            fs::metadata(dir.join(ID)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load(&dir, ID).unwrap(), cap);
        assert!(save(&dir, ID, "overwrite").is_err());
        remove_required(&dir, ID).unwrap();
        symlink(temp.path().join("target"), dir.join(ID)).unwrap();
        assert!(save(&dir, ID, "overwrite").is_err());
        assert!(load(&dir, ID).is_err());
        let link = temp.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }

    #[test]
    fn validate_names_and_request_ids() {
        assert!(env_name("API_TOKEN_2"));
        for name in ["", "1TOKEN", "A=B", "BAD-NAME", "bad\0name"] {
            assert!(!env_name(name));
        }
        assert!(valid_id(ID));
        assert!(!valid_id("../../secret"));
    }
}
