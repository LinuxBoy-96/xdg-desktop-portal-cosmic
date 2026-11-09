// SPDX-License-Identifier: GPL-3.0-only

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use cosmic::desktop::fde::{self, DesktopEntry};
use cosmic_launch::{CustomExecutable, EntryLauncher};
use zbus::OwnedVariant;

use crate::PortalResponse;

const MAILTO_MIME: &str = "x-scheme-handler/mailto";

#[derive(zbus::DeserializeDict, zbus::Type, Debug)]
#[zbus(signature = "a{sv}")]
struct ComposeEmailOptions {
    address: Option<String>,
    addresses: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    subject: Option<String>,
    body: Option<String>,
    attachments: Option<Vec<String>>,
    activation_token: Option<String>,
}

pub struct Email;

#[zbus::interface(name = "org.freedesktop.impl.portal.Email")]
impl Email {
    async fn compose_email(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: zbus::ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: ComposeEmailOptions,
    ) -> PortalResponse<HashMap<String, OwnedVariant>> {
        tracing::debug!("ComposeEmail {handle} {app_id} {parent_window}");
        match compose(connection, options).await {
            Ok(()) => PortalResponse::Success(HashMap::new()),
            Err(err) => {
                tracing::warn!("ComposeEmail: {err}");
                PortalResponse::Other
            }
        }
    }
}

async fn compose(
    connection: &zbus::Connection,
    options: ComposeEmailOptions,
) -> Result<(), String> {
    let message = Message {
        to: options
            .address
            .into_iter()
            .chain(options.addresses.into_iter().flatten())
            .collect(),
        cc: options.cc.unwrap_or_default(),
        bcc: options.bcc.unwrap_or_default(),
        subject: options.subject.unwrap_or_default(),
        body: options.body.unwrap_or_default(),
        attachments: options
            .attachments
            .unwrap_or_default()
            .iter()
            .map(|attachment| file_uri(attachment))
            .collect(),
    };

    let locales: Arc<[String]> = fde::get_languages_from_env().into();
    let entry = {
        let locales = locales.clone();
        tokio::task::spawn_blocking(move || mail_client(&locales))
            .await
            .map_err(|err| err.to_string())?
            .ok_or("no mailto handler")?
    };

    let mut env_vars = Vec::new();
    if let Some(token) = options.activation_token {
        env_vars.push(("XDG_ACTIVATION_TOKEN".to_owned(), token.clone()));
        env_vars.push(("DESKTOP_STARTUP_ID".to_owned(), token));
    }

    let result = if entry.appid.to_ascii_lowercase().contains("thunderbird") {
        let exec = entry.exec().unwrap_or_default();
        let exec = if exec.contains("%u") || exec.contains("%U") {
            exec.replace("%u", "-compose %u")
                .replace("%U", "-compose %u")
        } else {
            format!("{exec} -compose %u")
        };
        let launcher = CustomExecutable::new(entry.appid.clone(), exec);
        let fields = message.thunderbird_compose();
        cosmic_launch::spawn(connection, &launcher, None, &[], None, env_vars, &[fields]).await
    } else {
        let launcher = EntryLauncher::new(Arc::new(entry), locales);
        let url = message.mailto();
        cosmic_launch::spawn(connection, &launcher, None, &[], None, env_vars, &[url]).await
    };
    result.map_err(|err| err.to_string())
}

/// Default mail client from Settings or the first app handling `mailto:`
fn mail_client(locales: &[String]) -> Option<DesktopEntry> {
    let entries: Vec<DesktopEntry> = fde::Iter::new(fde::default_paths())
        .filter_map(|path| DesktopEntry::from_path(path, Some(locales)).ok())
        .collect();
    let mut list = cosmic_mime_apps::List::default();
    list.load_from_paths(&cosmic_mime_apps::list_paths());

    list.default_mail_client()
        .into_iter()
        .flatten()
        .find_map(|id| {
            let id: &str = id;
            let id = id.strip_suffix(".desktop").unwrap_or(id);
            entries.iter().find(|entry| entry.appid == id)
        })
        .or_else(|| {
            entries.iter().find(|entry| {
                entry
                    .mime_type()
                    .is_some_and(|mimes| mimes.contains(&MAILTO_MIME))
            })
        })
        .cloned()
}

struct Message {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
    attachments: Vec<String>,
}

impl Message {
    fn mailto(&self) -> String {
        let mut params = Vec::new();
        if !self.cc.is_empty() {
            params.push(("cc", join_addresses(&self.cc)));
        }
        if !self.bcc.is_empty() {
            params.push(("bcc", join_addresses(&self.bcc)));
        }
        if !self.subject.is_empty() {
            params.push(("subject", encode(&self.subject, b"")));
        }
        if !self.body.is_empty() {
            let body = self.body.replace("\r\n", "\n").replace('\n', "\r\n");
            params.push(("body", encode(&body, b"")));
        }
        for uri in &self.attachments {
            params.push(("attach", encode(uri, b"")));
        }

        let mut url = format!("mailto:{}", join_addresses(&self.to));
        for (i, (key, value)) in params.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(value);
        }
        url
    }

    /// Thunderbird refuses `mailto:` This the same method as KDE
    fn thunderbird_compose(&self) -> String {
        let quoted = |values: Vec<String>| format!("'{}'", values.join(","));
        let mut fields = Vec::new();
        for (key, addresses) in [("to", &self.to), ("cc", &self.cc), ("bcc", &self.bcc)] {
            if !addresses.is_empty() {
                let escaped = addresses
                    .iter()
                    .map(|address| address.trim().replace(',', "\\,").replace('\'', "\\'"))
                    .collect();
                fields.push(format!("{key}={}", quoted(escaped)));
            }
        }
        if !self.attachments.is_empty() {
            let uris = self
                .attachments
                .iter()
                .map(|uri| uri.replace(',', "%2C").replace('\'', "%27"))
                .collect();
            fields.push(format!("attachment={}", quoted(uris)));
        }
        if !self.subject.is_empty() {
            fields.push(format!("subject={}", encode(&self.subject, b"")));
        }
        if !self.body.is_empty() {
            fields.push(format!("body={}", encode(&html_escape(&self.body), b"")));
        }
        fields.join(",")
    }
}

fn file_uri(attachment: &str) -> String {
    if attachment.starts_with('/') {
        format!("file://{}", encode(attachment, b"/"))
    } else {
        attachment.to_owned()
    }
}

fn join_addresses(addresses: &[String]) -> String {
    addresses
        .iter()
        .map(|address| encode(address.trim(), b"@"))
        .collect::<Vec<_>>()
        .join(",")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn encode(value: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || keep.contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Message {
        let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
        Message {
            to: strings(&["a@x.org", " b?c@y.org "]),
            cc: strings(&["c@z.org"]),
            bcc: strings(&["d,e@w.org"]),
            subject: "Allô & ça? 100% + #1".into(),
            body: "ligne 1\n<b>ligne 2</b>".into(),
            attachments: vec![file_uri("/tmp/Mes docs/été,1.pdf")],
        }
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            file_uri("/tmp/Mes docs/été,1.pdf"),
            "file:///tmp/Mes%20docs/%C3%A9t%C3%A9%2C1.pdf"
        );
        assert_eq!(file_uri("file:///tmp/a.pdf"), "file:///tmp/a.pdf");
    }

    #[test]
    fn mailto() {
        assert_eq!(
            message().mailto(),
            "mailto:a@x.org,b%3Fc@y.org?cc=c@z.org&bcc=d%2Ce@w.org\
             &subject=All%C3%B4%20%26%20%C3%A7a%3F%20100%25%20%2B%20%231\
             &body=ligne%201%0D%0A%3Cb%3Eligne%202%3C%2Fb%3E\
             &attach=file%3A%2F%2F%2Ftmp%2FMes%2520docs%2F%25C3%25A9t%25C3%25A9%252C1.pdf"
        );
    }

    #[test]
    fn thunderbird() {
        assert_eq!(
            message().thunderbird_compose(),
            "to='a@x.org,b?c@y.org',cc='c@z.org',bcc='d\\,e@w.org',\
             attachment='file:///tmp/Mes%20docs/%C3%A9t%C3%A9%2C1.pdf',\
             subject=All%C3%B4%20%26%20%C3%A7a%3F%20100%25%20%2B%20%231,\
             body=ligne%201%0A%26lt%3Bb%26gt%3Bligne%202%26lt%3B%2Fb%26gt%3B"
        );
    }
}
