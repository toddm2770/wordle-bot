//! Command handling. Guesses and board state only ever go to the player's DM;
//! the public channel only sees acknowledgements, stats, leaderboards and
//! spoiler-free shares.

pub mod commands;
pub mod render;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::Error;
use crate::fluxer::gateway::GatewayEvent;
use crate::fluxer::model::{Message, OutgoingMessage};
use crate::fluxer::rest::{ApiError, RestClient};
use crate::game::GameMode;
use crate::service::{GameService, GameStatus, GuessOutcome, Player, ShareOutcome};
use commands::Command;

pub struct Bot {
    rest: RestClient,
    service: Arc<GameService>,
    prefix: String,
    leaderboard_size: usize,
    dm_channels: Mutex<HashMap<i64, String>>,
}

impl Bot {
    pub fn new(rest: RestClient, service: Arc<GameService>, prefix: String, leaderboard_size: usize) -> Bot {
        Bot { rest, service, prefix, leaderboard_size, dm_channels: Mutex::new(HashMap::new()) }
    }

    pub async fn handle_event(&self, event: GatewayEvent) {
        match event {
            GatewayEvent::Ready(r) => {
                tracing::info!(bot = %r.user.username, guilds = r.guilds.len(), "gateway session ready")
            }
            GatewayEvent::Resumed => tracing::info!("gateway session resumed"),
            GatewayEvent::GuildCreate(g) => {
                tracing::info!(guild_id = %g.id, name = g.name.as_deref().unwrap_or("?"), "joined/available guild")
            }
            GatewayEvent::MessageCreate(m) => self.handle_message(&m).await,
        }
    }

    async fn handle_message(&self, msg: &Message) {
        if msg.author.bot || msg.webhook_id.is_some() {
            return;
        }
        let in_dm = msg.guild_id.is_none();
        let command = match commands::parse(&msg.content, &self.prefix) {
            Some(c) => c,
            // In a DM, a bare five-letter word is treated as a guess.
            None if in_dm => match commands::bare_guess(&msg.content) {
                Some(c) => c,
                None => return,
            },
            None => return,
        };
        let Some(user_id) = msg.author.id_i64() else { return };
        let player = Player { user_id, display_name: msg.author_display_name() };
        let guild_id: Option<i64> = msg.guild_id.as_deref().and_then(|g| g.parse().ok());

        tracing::debug!(user_id, ?command, in_guild = guild_id.is_some(), "command received");
        let channel = msg.channel_id.as_str();

        match command {
            Command::Help => self.reply(channel, &render::help(&self.prefix)).await,
            Command::Usage(text) => self.reply(channel, &OutgoingMessage::text(text)).await,
            Command::Play(mode) => self.play(channel, &player, guild_id, mode).await,
            Command::Guess(word) => self.guess(msg, &player, guild_id.is_some(), &word).await,
            Command::Board => self.board(channel, &player, guild_id.is_some()).await,
            Command::Stats => self.stats(channel, &player).await,
            Command::Leaderboard(sort) => match guild_id {
                None => {
                    self.reply(channel, &OutgoingMessage::text("The leaderboard is per community. Run this command in a server channel.")).await
                }
                Some(g) => match self.service.leaderboard(g, sort).await {
                    Ok(entries) => {
                        let community = self.community(g).await;
                        let out = render::leaderboard(&entries, sort, self.leaderboard_size, &self.prefix, community.as_ref());
                        self.reply(channel, &out).await
                    }
                    Err(e) => self.internal_error(channel, &e).await,
                },
            },
            Command::Mode(mode) => self.set_mode(channel, &player, mode).await,
            Command::Share => self.share(channel, &player, guild_id).await,
            Command::Community => match guild_id {
                None => {
                    self.reply(channel, &OutgoingMessage::text("The community streak belongs to a server. Run this command in a server channel.")).await
                }
                Some(g) => match self.service.community_streak(g).await {
                    Ok(c) => self.reply(channel, &render::community_streak(&c, &self.prefix)).await,
                    Err(e) => self.internal_error(channel, &e).await,
                },
            },
        }
    }

    async fn play(&self, channel: &str, player: &Player, guild_id: Option<i64>, mode: Option<GameMode>) {
        if guild_id.is_none() {
            let text = format!(
                "Games are started from a server channel so you appear on that community's leaderboard. Run `{} play` there and I'll continue the game here in our DM.",
                self.prefix
            );
            return self.reply(channel, &OutgoingMessage::text(text)).await;
        }
        let outcome = match self.service.start_game(player, guild_id, mode).await {
            Ok(o) => o,
            Err(e) => return self.internal_error(channel, &e).await,
        };
        let view = &outcome.view;

        if view.status.is_finished() {
            let verdict = if view.status == GameStatus::Won { "solved" } else { "played" };
            let text = format!(
                "{}, you've already {verdict} today's puzzle (#{}). Use `{} share` to show your result, or come back after midnight (America/New_York).",
                player.display_name,
                render::format_number(view.puzzle_number),
                self.prefix
            );
            return self.reply(channel, &OutgoingMessage::text(text)).await;
        }

        let notice = outcome.mode_locked.then(|| {
            format!("Your mode is locked to **{}** for today because you've already made a guess.", view.mode)
        });
        let dm = render::board(view, notice.as_deref(), &self.prefix);
        match self.dm(player.user_id, &dm).await {
            Ok(()) => {
                let verb = if outcome.created { "is ready" } else { "is waiting" };
                let text = format!(
                    "📬 {}, today's puzzle (#{}, {} mode) {verb} in your DMs. Send your guesses there with `{} guess WORD`.",
                    player.display_name,
                    render::format_number(view.puzzle_number),
                    view.mode,
                    self.prefix
                );
                self.reply(channel, &OutgoingMessage::text(text)).await;
            }
            Err(e) => {
                tracing::warn!(user_id = player.user_id, error = %e, "could not DM player: {}", e.hint());
                let text = format!(
                    "{}, I couldn't send you a DM, so I can't show you the puzzle without spoiling it for others. Allow direct messages from members of this community in your privacy settings, then run `{} play` again.",
                    player.display_name, self.prefix
                );
                self.reply(channel, &OutgoingMessage::text(text)).await;
            }
        }
    }

    async fn guess(&self, msg: &Message, player: &Player, in_guild: bool, word: &str) {
        let channel = msg.channel_id.as_str();
        if in_guild {
            // The guess is already public; never score it, and remove it if we can.
            let deleted = self.rest.delete_message(channel, &msg.id).await;
            if let Err(e) = &deleted {
                tracing::warn!(error = %e, "could not delete a guess posted in a public channel: {}", e.hint());
            }
            let text = format!(
                "🔒 {}, guesses must be sent in a DM with me so nobody gets spoilers. {}That guess was not counted. Use `{} play` to open your DM.",
                player.display_name,
                if deleted.is_ok() { "I removed your message. " } else { "Please delete your message. " },
                self.prefix
            );
            return self.reply(channel, &OutgoingMessage::text(text)).await;
        }

        let message_id = msg.id.parse::<i64>().ok();
        match self.service.submit_guess(player.user_id, word, message_id).await {
            Ok(GuessOutcome::Duplicate) => tracing::debug!(message_id = %msg.id, "duplicate message ignored"),
            Ok(GuessOutcome::Rejected(r)) => self.reply(channel, &render::guess_rejected(&r, &self.prefix)).await,
            Ok(GuessOutcome::Accepted { view, stats }) => {
                let out = match (view.status, stats) {
                    (GameStatus::Won, Some(s)) => {
                        let community = match view.guild_id {
                            Some(g) => self.community(g).await,
                            None => None,
                        };
                        render::won(&view, &s, &self.prefix, community.as_ref())
                    }
                    (GameStatus::Lost, Some(s)) => render::lost(&view, &s),
                    _ => render::board(&view, None, &self.prefix),
                };
                self.reply(channel, &out).await;
            }
            Err(e) => self.internal_error(channel, &e).await,
        }
    }

    async fn board(&self, channel: &str, player: &Player, in_guild: bool) {
        let view = match self.service.board(player.user_id).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                return self
                    .reply(
                        channel,
                        &OutgoingMessage::text(format!("You haven't started today's puzzle. Run `{} play` in a server channel.", self.prefix)),
                    )
                    .await
            }
            Err(e) => return self.internal_error(channel, &e).await,
        };
        let notice = view.solution.as_ref().map(|s| format!("The word was **{}**.", s.to_ascii_uppercase()));
        let out = render::board(&view, notice.as_deref(), &self.prefix);

        if !in_guild {
            return self.reply(channel, &out).await;
        }
        match self.dm(player.user_id, &out).await {
            Ok(()) => {
                let text = format!("📬 {}, I sent your board to your DMs.", player.display_name);
                self.reply(channel, &OutgoingMessage::text(text)).await
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not DM board: {}", e.hint());
                let text = format!(
                    "{}, I couldn't DM you, and I won't show your board publicly. Check your privacy settings.",
                    player.display_name
                );
                self.reply(channel, &OutgoingMessage::text(text)).await
            }
        }
    }

    async fn stats(&self, channel: &str, player: &Player) {
        match self.service.stats(player.user_id).await {
            Ok(Some(s)) if s.games_played > 0 => self.reply(channel, &render::stats(&s)).await,
            Ok(_) => {
                let text = format!("{}, you haven't finished a puzzle yet. Run `{} play` to start!", player.display_name, self.prefix);
                self.reply(channel, &OutgoingMessage::text(text)).await
            }
            Err(e) => self.internal_error(channel, &e).await,
        }
    }

    async fn set_mode(&self, channel: &str, player: &Player, mode: GameMode) {
        match self.service.set_preferred_mode(player, mode).await {
            Ok(change) => {
                let mut text = format!("{}, your preferred mode is now **{}**.", player.display_name, change.preferred);
                if change.applied_to_today {
                    text.push_str(" Today's game was switched too, since you haven't guessed yet.");
                }
                if change.locked_today {
                    text.push_str(" Today's game keeps its current mode because your first guess was already accepted; the new mode applies from tomorrow.");
                }
                self.reply(channel, &OutgoingMessage::text(text)).await
            }
            Err(e) => self.internal_error(channel, &e).await,
        }
    }

    /// Community streak for decorating other messages; failures are logged, not shown.
    async fn community(&self, guild_id: i64) -> Option<crate::service::CommunityStreak> {
        match self.service.community_streak(guild_id).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(guild_id, error = %e, "could not load community streak");
                None
            }
        }
    }

    async fn share(&self, channel: &str, player: &Player, guild_id: Option<i64>) {
        let Some(guild) = guild_id else {
            return self
                .reply(channel, &OutgoingMessage::text(format!("Run `{} share` in a server channel to post your result there.", self.prefix)))
                .await;
        };
        let text = match self.service.share(player.user_id).await {
            Ok(ShareOutcome::NoGame) => format!("You haven't played today's puzzle yet. Run `{} play`.", self.prefix),
            Ok(ShareOutcome::InProgress) => "Finish today's puzzle before sharing your result.".to_string(),
            Ok(ShareOutcome::Ready(data)) => {
                let community = self.community(guild).await;
                render::share_text(&player.display_name, &data, community.as_ref())
            }
            Err(e) => return self.internal_error(channel, &e).await,
        };
        self.reply(channel, &OutgoingMessage::text(text)).await
    }

    async fn internal_error(&self, channel: &str, e: &Error) {
        tracing::error!(error = %e, "command failed");
        let text = match e {
            Error::PuzzleUnavailable(_) => {
                "Today's puzzle couldn't be loaded right now. Nothing was lost; please try again in a minute."
            }
            _ => "Something went wrong on my side. Please try again shortly.",
        };
        self.reply(channel, &OutgoingMessage::text(text)).await
    }

    /// Send to a channel; falls back to plain text if embeds are not permitted,
    /// and logs actionable hints when the bot cannot post at all.
    async fn reply(&self, channel: &str, message: &OutgoingMessage) {
        let mut result = self.rest.send_message(channel, message).await;
        if let Err(e) = &result {
            if e.is_missing_permissions() && !message.embeds.is_empty() {
                tracing::warn!(%channel, "embed rejected, retrying as plain text: {}", e.hint());
                result = self.rest.send_message(channel, &OutgoingMessage::text(message.to_plain_text())).await;
            }
        }
        if let Err(e) = result {
            tracing::error!(%channel, error = %e, "failed to send message: {}", e.hint());
        }
    }

    /// Send to a user's DM, opening the channel on first use.
    async fn dm(&self, user_id: i64, message: &OutgoingMessage) -> Result<(), ApiError> {
        for attempt in 0..2 {
            let cached = self.dm_channels.lock().await.get(&user_id).cloned();
            let channel = match cached {
                Some(c) => c,
                None => {
                    let c = self.rest.open_dm(&user_id.to_string()).await?;
                    self.dm_channels.lock().await.insert(user_id, c.clone());
                    c
                }
            };
            match self.rest.send_message(&channel, message).await {
                Ok(()) => return Ok(()),
                Err(e) if attempt == 0 && e.status() == Some(404) => {
                    self.dm_channels.lock().await.remove(&user_id);
                }
                Err(e) => return Err(e),
            }
        }
        Err(ApiError::Transport("DM channel could not be opened".into()))
    }
}
