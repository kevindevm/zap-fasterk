//! WhatsApp click-to-chat links and launch numbers. Opening one prepares a draft, never a send.

/// A validated recipient and optional URL-decoded draft.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatLink {
    phone: String,
    pub text: Option<String>,
}

// Actions and command-line diagnostics must not disclose the link's contents.
impl std::fmt::Debug for ChatLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatLink { recipient and draft redacted }")
    }
}

impl ChatLink {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let value = value.trim();
        if !value.contains([':', '/']) {
            return Self::validated(value, None);
        }
        let value = if value.starts_with("wa.me/") {
            format!("https://{value}")
        } else {
            value.to_owned()
        };
        let url = reqwest::Url::parse(&value).map_err(|_| "Invalid WhatsApp chat link")?;
        let send = match url.host_str() {
            Some(host) => host.eq_ignore_ascii_case("send") && matches!(url.path(), "" | "/"),
            None => url.path() == "send",
        };
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            return Err("Invalid WhatsApp chat link");
        }
        let path_phone = match (url.scheme(), url.host_str()) {
            ("whatsapp", _) if send && url.fragment().is_none() => None,
            ("http" | "https", Some("wa.me" | "www.wa.me")) => {
                let path = url.path().trim_start_matches('/').trim_end_matches('/');
                if path.is_empty() || path.contains('/') {
                    return Err("The WhatsApp chat link needs an international phone number");
                }
                Some(path)
            }
            ("http" | "https", Some("api.whatsapp.com" | "web.whatsapp.com"))
                if matches!(url.path(), "/send" | "/send/") =>
            {
                None
            }
            _ => return Err("Unsupported WhatsApp chat link"),
        };
        let mut phone = None;
        let mut text = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "phone" if phone.is_none() => phone = Some(value.into_owned()),
                "text" if text.is_none() => text = Some(value.into_owned()),
                "phone" | "text" => return Err("Ambiguous WhatsApp chat link"),
                _ => {}
            }
        }
        // A wa.me path names its recipient. A conflicting query must not quietly
        // open another chat, and query digits must never join the path's number.
        let phone = if let Some(path) = path_phone {
            let path = Self::validated(path, None)?;
            if let Some(query) = phone
                && Self::validated(&query, None)?.phone != path.phone
            {
                return Err("Ambiguous WhatsApp chat link");
            }
            path.phone
        } else {
            phone.ok_or("The WhatsApp chat link needs an international phone number")?
        };
        Self::validated(&phone, text)
    }

    fn validated(phone: &str, text: Option<String>) -> Result<Self, &'static str> {
        let phone = phone.trim().trim_start_matches('+');
        if !phone.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, ' ' | '-' | '(' | ')' | '.')
        }) {
            return Err("The WhatsApp chat link needs an international phone number");
        }
        let phone: String = phone.chars().filter(char::is_ascii_digit).collect();
        if !(6..=15).contains(&phone.len()) || phone.starts_with('0') {
            return Err("The WhatsApp chat link needs an international phone number");
        }
        let link = Self { phone, text };
        // fastframe-instance accepts a 16 KiB line, including its prefix and
        // the Windows token. Leave room for those instead of losing a draft
        // only when Zap Fasterk was already running.
        if link.request().len() > 15 * 1024 {
            return Err("The WhatsApp chat link is too long");
        }
        Ok(link)
    }

    pub fn chat_id(&self) -> crate::model::ChatId {
        format!("{}@s.whatsapp.net", self.phone)
    }

    /// URL encoding keeps even multiline drafts inside one private IPC line.
    pub fn request(&self) -> String {
        let mut url = reqwest::Url::parse("whatsapp://send").expect("a fixed WhatsApp URL");
        url.query_pairs_mut().append_pair("phone", &self.phone);
        if let Some(text) = &self.text {
            url.query_pairs_mut().append_pair("text", text);
        }
        format!("open-link {url}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_links_preserve_the_recipient_and_decoded_draft() {
        let link = ChatLink::parse(
            "whatsapp://send/?phone=15550100123&text=Hola%2C+informaci%C3%B3n+%26+precio+%F0%9F%98%8A%0Ahttps%3A%2F%2Fexample.com%2F%3Fa%3Db%26c%3Dd&app_absent=0",
        )
        .unwrap();
        assert_eq!(link.chat_id(), "15550100123@s.whatsapp.net");
        assert_eq!(
            link.text.as_deref(),
            Some("Hola, información & precio 😊\nhttps://example.com/?a=b&c=d")
        );
    }

    #[test]
    fn links_without_a_draft_and_encoded_plus_signs_are_understood() {
        let link = ChatLink::parse("whatsapp:send?phone=%2B15550100123").unwrap();
        assert_eq!(link.chat_id(), "15550100123@s.whatsapp.net");
        assert_eq!(link.text, None);
        let link = ChatLink::parse("whatsapp://send?phone=15550100123&text=a%2Bb%2520c").unwrap();
        assert_eq!(link.text.as_deref(), Some("a+b%20c"));
    }

    #[test]
    fn invalid_or_ambiguous_links_do_not_name_a_recipient() {
        for value in [
            "https://example.com/send?phone=15550100123",
            "whatsapp://call?phone=15550100123",
            "whatsapp://send/path?phone=15550100123",
            "whatsapp://send@elsewhere?phone=15550100123",
            "whatsapp://send:1234?phone=15550100123",
            "whatsapp://send?text=Hello",
            "whatsapp://send?phone=",
            "whatsapp://send?phone=phone",
            "whatsapp://send?phone=0123",
            "whatsapp://send?phone=1234567890123456",
            "whatsapp://send?phone=15550100123&phone=15550100124",
            "whatsapp://send?phone=15550100123&text=one&text=two",
            "whatsapp://send?phone=15550100123#fragment",
        ] {
            assert!(ChatLink::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn private_requests_round_trip_without_newlines_or_debug_payloads() {
        let link =
            ChatLink::parse("whatsapp://send?phone=15550100123&text=one%0Atwo%0Dthree").unwrap();
        let request = link.request();
        assert!(!request.contains(['\n', '\r']));
        assert_eq!(
            ChatLink::parse(request.strip_prefix("open-link ").unwrap()).unwrap(),
            link
        );
        let debug = format!("{link:?}");
        assert!(!debug.contains("15550100123"));
        assert!(!debug.contains("three"));
    }

    #[test]
    fn oversized_drafts_are_refused_before_a_launch_or_handoff() {
        let link = format!(
            "whatsapp://send?phone=15550100123&text={}",
            "a".repeat(16 * 1024)
        );
        assert!(ChatLink::parse(&link).is_err());
    }

    #[test]
    fn launch_numbers_and_web_links_use_the_existing_private_handoff() {
        for value in [
            "15550100123",
            "+1 (555) 010-0123",
            "https://wa.me/15550100123",
            "http://www.wa.me/15550100123/",
            "wa.me/15550100123",
            "https://api.whatsapp.com/send?phone=15550100123",
            "https://web.whatsapp.com/send?phone=%2B15550100123",
        ] {
            let link = ChatLink::parse(value).unwrap();
            assert_eq!(link.chat_id(), "15550100123@s.whatsapp.net", "{value}");
            assert_eq!(
                ChatLink::parse(link.request().strip_prefix("open-link ").unwrap()).unwrap(),
                link,
            );
        }
        let link =
            ChatLink::parse("https://wa.me/15550100123?text=Hello+%23team%0A%F0%9F%98%8A#section")
                .unwrap();
        assert_eq!(link.text.as_deref(), Some("Hello #team\n😊"));
    }

    #[test]
    fn web_targets_cannot_substitute_or_invent_a_recipient() {
        for value in [
            "https://wa.me/2012?text=999999",
            "https://wa.me/?phone=15550100123",
            "https://wa.me/15550100123?phone=15550100124",
            "https://wa.me/15550100123?text=one&text=two",
            "https://wa.me/extra/15550100123",
            "https://wa.me@evil.test/15550100123",
            "https://api.whatsapp.com/other?phone=15550100123",
            "https://example.com/?phone=15550100123",
            "call 15550100123",
            "15550100123 please",
            "2026",
        ] {
            assert!(ChatLink::parse(value).is_err(), "{value}");
        }
    }
}

/// A validated group invite. Debug output never includes its access code.
#[derive(Clone, PartialEq, Eq)]
pub struct GroupInviteLink(String);
impl std::fmt::Debug for GroupInviteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupInviteLink { code redacted }")
    }
}
impl GroupInviteLink {
    pub fn parse(value: &str) -> Option<Self> {
        crate::safety::group_invite_code(value).map(Self)
    }
    pub fn request(&self) -> String {
        format!("invite {}", self.0)
    }
    pub fn url(&self) -> String {
        format!("https://chat.whatsapp.com/{}", self.0)
    }
}
