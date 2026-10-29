//! Minimal Fluxer wire types: only the fields the bot reads.
//! Snowflakes arrive as decimal strings and fit in a signed 64-bit integer.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn id_i64(&self) -> Option<i64> {
        self.id.parse().ok()
    }

    pub fn display_name(&self) -> String {
        self.global_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.username)
            .to_string()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub nick: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub content: String,
    pub author: User,
    #[serde(default)]
    pub member: Option<Member>,
    /// 0 = guild text, 1 = DM.
    #[serde(default)]
    pub channel_type: Option<i32>,
    #[serde(default)]
    pub webhook_id: Option<String>,
}

impl Message {
    /// Guild nickname, falling back to the account display name.
    pub fn author_display_name(&self) -> String {
        self.member
            .as_ref()
            .and_then(|m| m.nick.as_deref())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.author.display_name())
    }

    pub fn is_dm(&self) -> bool {
        self.guild_id.is_none() && self.channel_type == Some(1)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ready {
    pub session_id: String,
    pub user: User,
    #[serde(default)]
    pub guilds: Vec<GuildStub>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GuildStub {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub unavailable: Option<bool>,
}

/// The bot's own application, from `GET /v1/applications/@me`.
#[derive(Clone, Debug, Deserialize)]
pub struct BotApplication {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub bot_public: bool,
    /// The account that owns the application (allowed to change bot settings).
    #[serde(default)]
    pub owner: Option<User>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Embed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<EmbedFooter>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EmbedField>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EmbedFooter {
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OutgoingMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<Embed>,
}

impl OutgoingMessage {
    pub fn text(content: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage { content: Some(content.into()), embeds: Vec::new() }
    }

    pub fn embed(embed: Embed) -> OutgoingMessage {
        OutgoingMessage { content: None, embeds: vec![embed] }
    }

    /// Plain-text rendition used when the bot cannot send embeds.
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.content {
            out.push_str(c);
        }
        for e in &self.embeds {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            if let Some(t) = &e.title {
                out.push_str(&format!("**{t}**\n"));
            }
            if let Some(d) = &e.description {
                out.push_str(d);
            }
            for f in &e.fields {
                out.push_str(&format!("\n**{}**\n{}", f.name, f.value));
            }
            if let Some(f) = &e.footer {
                out.push_str(&format!("\n_{}_", f.text));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_create_payload() {
        let json = r#"{
            "id":"1100","channel_id":"22","guild_id":"33","content":"!wordle play","type":0,
            "author":{"id":"44","username":"alice","discriminator":"0001","global_name":"Alice","avatar":null,"flags":0},
            "member":{"nick":"Al","roles":[],"joined_at":"2026-01-01T00:00:00Z"},
            "channel_type":0,"timestamp":"2026-01-01T00:00:00Z","embeds":[],"attachments":[]
        }"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.author.id_i64(), Some(44));
        assert_eq!(m.author_display_name(), "Al");
        assert!(!m.is_dm());
        assert!(!m.author.bot);
    }

    #[test]
    fn dm_message_has_no_guild() {
        let json = r#"{"id":"1","channel_id":"2","content":"x","channel_type":1,
            "author":{"id":"3","username":"bob","global_name":null}}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert!(m.is_dm());
        assert_eq!(m.author_display_name(), "bob");
    }

    #[test]
    fn parses_ready_with_unavailable_guild_stubs() {
        let json = r#"{"session_id":"abc","version":1,"user":{"id":"9","username":"wordle","bot":true},
            "guilds":[{"id":"5","unavailable":true}],"private_channels":[]}"#;
        let r: Ready = serde_json::from_str(json).unwrap();
        assert_eq!(r.guilds.len(), 1);
        assert!(r.user.bot);
    }

    #[test]
    fn outgoing_message_skips_empty_fields() {
        let m = OutgoingMessage::embed(Embed { title: Some("T".into()), ..Default::default() });
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"embeds":[{"title":"T"}]}"#);
    }

    #[test]
    fn plain_text_fallback_contains_all_parts() {
        let m = OutgoingMessage::embed(Embed {
            title: Some("Title".into()),
            description: Some("Body".into()),
            fields: vec![EmbedField { name: "N".into(), value: "V".into(), inline: false }],
            footer: Some(EmbedFooter { text: "foot".into() }),
            ..Default::default()
        });
        let t = m.to_plain_text();
        assert!(t.contains("**Title**") && t.contains("Body") && t.contains("**N**") && t.contains("foot"));
    }
}
