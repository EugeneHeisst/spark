use crate::error::{ApiError, ApiResult};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr},
    path::{Component, Path},
};

pub fn name(value: &str, max: usize) -> ApiResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > max || trimmed.chars().any(char::is_control)
    {
        return Err(ApiError::bad(format!(
            "Имя должно содержать от 1 до {max} символов без управляющих символов"
        )));
    }
    Ok(trimmed.into())
}
pub fn code(value: &str) -> ApiResult<()> {
    if value.chars().count() < 6 || value.len() > 256 || value.chars().any(char::is_control) {
        Err(ApiError::bad(
            "Код комнаты должен содержать от 6 до 256 символов",
        ))
    } else {
        Ok(())
    }
}
pub fn key(code: &str) -> String {
    hex::encode(Sha256::digest(code.as_bytes()))
}
pub fn equal_secret(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}
pub fn id(value: &str) -> ApiResult<()> {
    uuid::Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| ApiError::bad("Некорректный идентификатор"))
}
pub fn filename(value: &str) -> ApiResult<String> {
    if value.is_empty()
        || value.len() > 240
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|c| c.is_control() || "/\\<>:\"|?*".contains(c))
        || value.ends_with([' ', '.'])
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::bad("Недопустимое имя файла"));
    }
    let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        return Err(ApiError::bad("Зарезервированное имя файла"));
    }
    Ok(value.into())
}
pub fn address(value: &str) -> ApiResult<String> {
    let value = value.trim();
    // A network endpoint, never a URL, credentials, Unix socket, or filesystem path.
    if value.is_empty()
        || value.len() > 300
        || value
            .chars()
            .any(|c| c.is_whitespace() || "/\\@?#%".contains(c))
        || value.contains("://")
    {
        return Err(ApiError::bad(
            "Укажите IP-адрес или имя компьютера с портом, например 192.168.1.10:39393",
        ));
    }
    if let Ok(socket) = value.parse::<SocketAddr>() {
        if socket.port() == 0 || socket.ip().is_unspecified() || socket.ip().is_multicast() {
            return Err(ApiError::bad("Недопустимый адрес сервера"));
        }
        return Ok(socket.to_string());
    }
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| ApiError::bad("Не указан порт сервера"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| ApiError::bad("Некорректный порт сервера"))?;
    if port == 0
        || host.is_empty()
        || host.len() > 253
        || host.split('.').any(|part| {
            part.is_empty()
                || part.len() > 63
                || part.starts_with('-')
                || part.ends_with('-')
                || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return Err(ApiError::bad("Недопустимое имя сервера"));
    }
    if host
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified() || ip.is_multicast())
    {
        return Err(ApiError::bad("Недопустимый адрес сервера"));
    }
    Ok(format!("{host}:{port}"))
}
pub fn mime(value: &str) -> String {
    if value.len() <= 128
        && !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        value.into()
    } else {
        "application/octet-stream".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_paths_credentials_and_reserved_names() {
        for bad in [
            "../x",
            "..\\x",
            "x/y",
            "x:y",
            "CON.txt",
            "NUL",
            "x\r\nheader",
            "x.",
        ] {
            assert!(filename(bad).is_err(), "{bad}");
        }
        for bad in [
            "http://127.0.0.1:80",
            "user@host:80",
            "host:1/path",
            "host:0",
            "0.0.0.0:80",
            "host:80?key=x",
        ] {
            assert!(address(bad).is_err(), "{bad}");
        }
        assert_eq!(filename("фото 2026.png").unwrap(), "фото 2026.png");
        assert_eq!(address("192.168.1.4:39393").unwrap(), "192.168.1.4:39393");
        assert_eq!(address("[::1]:39393").unwrap(), "[::1]:39393");
    }
}
