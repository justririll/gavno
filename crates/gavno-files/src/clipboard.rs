use gio::prelude::*;
use gtk::gdk;

pub const GNOME: &str = "x-special/gnome-copied-files";
pub const URI_LIST: &str = "text/uri-list";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files {
    pub cut: bool,
    pub uris: Vec<String>,
}

fn valid_uri(uri: &str) -> bool {
    !uri.contains(['\0', '\r', '\n'])
        && uri.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty()
                && scheme.as_bytes()[0].is_ascii_alphabetic()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        })
}
impl Files {
    pub fn parse(text: &str, gnome: bool) -> Result<Self, String> {
        let mut lines = text.lines();
        let cut = if gnome {
            match lines.next() {
                Some("cut") => true,
                Some("copy") => false,
                _ => return Err("Invalid clipboard operation".into()),
            }
        } else {
            false
        };
        let mut uris = Vec::new();
        for line in lines {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !valid_uri(line) {
                return Err("Invalid clipboard URI".into());
            }
            uris.push(line.to_string());
        }
        if uris.is_empty() {
            return Err("The clipboard contains no files".into());
        }
        Ok(Self { cut, uris })
    }
    pub fn gnome(&self) -> String {
        format!(
            "{}\n{}\n",
            if self.cut { "cut" } else { "copy" },
            self.uris.join("\n")
        )
    }
    pub fn uri_list(&self) -> String {
        format!("{}\r\n", self.uris.join("\r\n"))
    }
    pub fn provider(&self) -> gdk::ContentProvider {
        gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_bytes(
                GNOME,
                &glib::Bytes::from_owned(self.gnome().into_bytes()),
            ),
            gdk::ContentProvider::for_bytes(
                URI_LIST,
                &glib::Bytes::from_owned(self.uri_list().into_bytes()),
            ),
        ])
    }
}
pub async fn read(clipboard: &gdk::Clipboard) -> Result<Files, String> {
    let (stream, mime) = clipboard
        .read_future(&[GNOME, URI_LIST], glib::Priority::DEFAULT)
        .await
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    loop {
        let part = stream
            .read_bytes_future(8192, glib::Priority::DEFAULT)
            .await
            .map_err(|e| e.to_string())?;
        if part.is_empty() {
            break;
        }
        bytes.extend_from_slice(&part);
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Clipboard file list is too large".into());
        }
    }
    stream
        .close_future(glib::Priority::DEFAULT)
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    Files::parse(&text, mime == GNOME)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_roundtrip() {
        for cut in [false, true] {
            let files = Files {
                cut,
                uris: vec!["file:///tmp/a%20b".into(), "file:///tmp/%E6%96%87".into()],
            };
            assert_eq!(Files::parse(&files.gnome(), true).expect("parse"), files);
            let mut copied = files.clone();
            copied.cut = false;
            assert_eq!(
                Files::parse(&files.uri_list(), false).expect("uri list"),
                copied
            );
        }
    }
    #[test]
    fn clipboard_validation() {
        assert!(Files::parse("move\nfile:///a", true).is_err());
        assert!(Files::parse("cut\n", true).is_err());
        assert!(Files::parse("copy\n/relative", true).is_err());
        assert!(Files::parse("copy\nfile:///a\0b", true).is_err());
        assert_eq!(
            Files::parse("# comment\r\nfile:///a\r\n", false)
                .expect("comments")
                .uris,
            ["file:///a"]
        );
    }
}
