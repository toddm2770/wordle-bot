//! Command handling.
//!
//! Happy path: `!wordle play` in a server channel → the puzzle arrives by DM
//! (nothing is posted in the channel) → guesses in the DM → when the game ends,
//! the spoiler-free grid is posted automatically in the community's results
//! channel. On later days the player can just type a word in the DM.
//!
//! Private things (boards, help, usage hints, errors about your own game) go to
//! DMs. The channel only gets results, leaderboards, stats and community streaks,
//! plus a short notice when a DM could not be delivered.

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
use crate::service::{
    CommunityStreak, GameService, GameStatus, GameView, GuessOutcome, Origin, Player, Rejection, ShareData,
    ShareOutcome,
};
use commands::{ChannelAction, Command};

pub struct BotOptions {
    pub prefix: String,
    pub leaderboard_size: usize,
    /// Replace each day's individual result posts with one recap after midnight.
    pub compact_results: bool,
    /// Owner of the bot application; may change settings in any community.
    pub app_owner_id: Option<i64>,
}

pub struct Bot {
    rest: RestClient,
    service: Arc<GameService>,
    prefix: String,
    leaderboard_size: usize,
    compact_results: bool,
    app_owner_id: Option<i64>,
    dm_channels: Mutex<HashMap<i64, String>>,
    guild_owners: Mutex<HashMap<i64, i64>>,
    recap_lock: Mutex<()>,
}

/// Where a reply should go.
#[derive(Copy, Clone)]
struct Ctx<'a> {
    channel: &'a str,
    guild_id: Option<i64>,
}

impl Ctx<'_> {
    fn in_dm(&self) -> bool {
        self.guild_id.is_none()
    }
}

impl Bot {
    pub fn new(rest: RestClient, service: Arc<GameService>, options: BotOptions) -> Bot {
        Bot {
            rest,
            service,
            prefix: options.prefix,
            leaderboard_size: options.leaderboard_size,
            compact_results: options.compact_results,
            app_owner_id: options.app_owner_id,
            dm_channels: Mutex::new(HashMap::new()),
            guild_owners: Mutex::new(HashMap::new()),
            recap_lock: Mutex::new(()),
        }
    }

    pub async fn handle_event(&self, event: GatewayEvent) {
        match event {
            GatewayEvent::Ready(r) => {
                tracing::info!(bot = %r.user.username, guilds = r.guilds.len(), "gateway session ready")
            }
            GatewayEvent::Resumed => tracing::info!("gateway session resumed"),
            GatewayEvent::GuildCreate(g) => {
                if let (Ok(id), Some(Ok(owner))) = (g.id.parse::<i64>(), g.owner_id.as_deref().map(str::parse::<i64>)) {
                    self.guild_owners.lock().await.insert(id, owner);
                }
                tracing::info!(guild_id = %g.id, name = g.name.as_deref().unwrap_or("?"), "joined/available guild")
            }
            GatewayEvent::MessageCreate(m) => self.handle_message(&m).await,
        }
    }

    async fn handle_message(&self, msg: &Message) {
        if msg.author.bot || msg.webhook_id.is_some() {
            return;
        }
        let guild_id: Option<i64> = msg.guild_id.as_deref().and_then(|g| g.parse().ok());
        let command = match commands::parse(&msg.content, &self.prefix) {
            Some(c) => c,
            // In a DM, a bare five-letter word is treated as a guess.
            None if guild_id.is_none() => match commands::bare_guess(&msg.content) {
                Some(c) => c,
                None => return,
            },
            None => return,
        };
        let Some(user_id) = msg.author.id_i64() else { return };
        let player = Player { user_id, display_name: msg.author_display_name() };
        let ctx = Ctx { channel: msg.channel_id.as_str(), guild_id };
        tracing::debug!(user_id, ?command, in_guild = guild_id.is_some(), "command received");

        match command {
            Command::Help => self.private(ctx, &player, &render::help(&self.prefix)).await,
            Command::Usage(text) => self.private(ctx, &player, &OutgoingMessage::text(text)).await,
            Command::Play(mode) => self.play(ctx, msg, &player, mode).await,
            Command::Guess(word) => self.guess(ctx, msg, &player, &word).await,
            Command::Board => self.board(ctx, &player).await,
            Command::Stats => self.stats(ctx, &player).await,
            Command::Mode(mode) => self.set_mode(ctx, &player, mode).await,
            Command::Leaderboard(sort) => {
                let Some(g) = self.require_guild(ctx, "The leaderboard").await else { return };
                match self.service.leaderboard(g, sort).await {
                    Ok(entries) => {
                        let community = self.community(g).await;
                        let out =
                            render::leaderboard(&entries, sort, self.leaderboard_size, &self.prefix, community.as_ref());
                        self.reply(ctx.channel, &out).await
                    }
                    Err(e) => self.internal_error(ctx.channel, &e).await,
                }
            }
            Command::Community => {
                let Some(g) = self.require_guild(ctx, "The community streak").await else { return };
                match self.service.community_streak(g).await {
                    Ok(c) => self.reply(ctx.channel, &render::community_streak(&c, &self.prefix)).await,
                    Err(e) => self.internal_error(ctx.channel, &e).await,
                }
            }
            Command::Share => self.share(ctx, &player).await,
            Command::Channel(action) => self.channel_setting(ctx, &player, action).await,
        }
    }

    async fn require_guild(&self, ctx: Ctx<'_>, what: &str) -> Option<i64> {
        if ctx.guild_id.is_none() {
            let text = format!("{what} belongs to a server. Run this command in a server channel.");
            self.reply(ctx.channel, &OutgoingMessage::text(text)).await;
        }
        ctx.guild_id
    }

    // ------------------------------------------------------------------ play

    async fn play(&self, ctx: Ctx<'_>, msg: &Message, player: &Player, mode: Option<GameMode>) {
        let origin = match ctx.guild_id {
            Some(guild_id) => Origin::Server { guild_id, channel_id: msg.channel_id.parse().ok() },
            None => Origin::Dm,
        };
        let outcome = match self.service.start_game_at(player, origin, mode).await {
            Ok(o) => o,
            Err(Error::NoHomeCommunity) => return self.reply(ctx.channel, &self.no_home_message()).await,
            Err(e) => return self.private_error(ctx, player, &e).await,
        };
        let view = &outcome.view;

        let message = if view.status.is_finished() {
            let verdict = if view.status == GameStatus::Won { "solved" } else { "played" };
            OutgoingMessage::text(format!(
                "You've already {verdict} today's puzzle (#{}). A new one unlocks at midnight (America/New_York); just type a word here to play it.",
                render::format_number(view.puzzle_number),
            ))
        } else {
            let notice = outcome
                .mode_locked
                .then(|| format!("Your mode is locked to **{}** for today because you've already made a guess.", view.mode));
            render::board(view, notice.as_deref(), &self.prefix)
        };
        // In a server, everything goes to the DM and the channel stays quiet.
        self.private(ctx, player, &message).await;
    }

    fn no_home_message(&self) -> OutgoingMessage {
        OutgoingMessage::text(format!(
            "Start your first game from a server channel with `{} play`, so I know which community to post your results in. After that you can play every day right here.",
            self.prefix
        ))
    }

    // ------------------------------------------------------------------ guesses

    async fn guess(&self, ctx: Ctx<'_>, msg: &Message, player: &Player, word: &str) {
        if !ctx.in_dm() {
            // The guess is already public; never score it, and remove it if we can.
            let deleted = self.rest.delete_message(ctx.channel, &msg.id).await;
            if let Err(e) = &deleted {
                tracing::warn!(error = %e, "could not delete a guess posted in a public channel: {}", e.hint());
            }
            let text = format!(
                "🔒 Guesses go in your DM with me so nobody gets spoilers. {}That guess was not counted.",
                if deleted.is_ok() { "I removed your message from the channel. " } else { "Please delete your message from the channel. " },
            );
            return self.private(ctx, player, &OutgoingMessage::text(text)).await;
        }

        let message_id = msg.id.parse::<i64>().ok();
        let mut outcome = self.service.submit_guess(player.user_id, word, message_id).await;

        // No game today yet: start one from the DM (previous community) and retry.
        if let Ok(GuessOutcome::Rejected(Rejection::NoGame)) = outcome {
            match self.service.start_game_at(player, Origin::Dm, None).await {
                Ok(_) => outcome = self.service.submit_guess(player.user_id, word, message_id).await,
                Err(Error::NoHomeCommunity) => return self.reply(ctx.channel, &self.no_home_message()).await,
                Err(e) => return self.internal_error(ctx.channel, &e).await,
            }
        }

        match outcome {
            Ok(GuessOutcome::Duplicate) => tracing::debug!(message_id = %msg.id, "duplicate message ignored"),
            Ok(GuessOutcome::Rejected(r)) => self.reply(ctx.channel, &render::guess_rejected(&r, &self.prefix)).await,
            Ok(GuessOutcome::Accepted { view, stats }) => match (view.status, stats) {
                (GameStatus::InProgress, _) | (_, None) => {
                    self.reply(ctx.channel, &render::board(&view, None, &self.prefix)).await
                }
                (status, Some(s)) => {
                    let community = match view.guild_id {
                        Some(g) => self.community(g).await,
                        None => None,
                    };
                    let target = self.results_target(&view).await;
                    let dm = if status == GameStatus::Won {
                        render::won(&view, &s, community.as_ref(), target.is_some())
                    } else {
                        render::lost(&view, &s, target.is_some())
                    };
                    self.reply(ctx.channel, &dm).await;
                    if let Some(channel) = target {
                        self.post_result(&view, player, channel, community.as_ref()).await;
                    }
                }
            },
            Err(e) => self.internal_error(ctx.channel, &e).await,
        }
    }

    /// The server channel a finished game's result goes to.
    async fn results_target(&self, view: &GameView) -> Option<i64> {
        let guild = view.guild_id?;
        match self.service.results_channel(guild).await {
            Ok(Some(c)) => Some(c),
            Ok(None) => view.channel_id,
            Err(e) => {
                tracing::warn!(guild, error = %e, "could not read results channel");
                view.channel_id
            }
        }
    }

    /// Post the spoiler-free grid publicly and remember it for the daily recap.
    async fn post_result(&self, view: &GameView, player: &Player, channel: i64, community: Option<&CommunityStreak>) {
        let Some(guild) = view.guild_id else { return };
        if matches!(self.service.result_posted(view.game_id).await, Ok(true)) {
            return;
        }
        let data = ShareData {
            puzzle_number: view.puzzle_number,
            mode: view.mode,
            won: view.status == GameStatus::Won,
            guesses_used: view.guesses.len(),
            rows: view.guesses.iter().map(|g| g.feedback).collect(),
        };
        let text = render::share_text(&player.display_name, &data, community);
        match self.rest.send_message(&channel.to_string(), &OutgoingMessage::text(text)).await {
            Ok(id) => {
                if let Ok(message_id) = id.parse::<i64>() {
                    if let Err(e) =
                        self.service.record_result_post(guild, view.puzzle_date, channel, message_id, view.game_id).await
                    {
                        tracing::warn!(error = %e, "could not record result post");
                    }
                }
            }
            Err(e) => tracing::error!(channel, error = %e, "could not post result: {}", e.hint()),
        }
    }

    // ------------------------------------------------------------------ other commands

    async fn board(&self, ctx: Ctx<'_>, player: &Player) {
        let message = match self.service.board(player.user_id).await {
            Ok(Some(view)) => {
                let notice = view.solution.as_ref().map(|s| format!("The word was **{}**.", s.to_ascii_uppercase()));
                render::board(&view, notice.as_deref(), &self.prefix)
            }
            Ok(None) => OutgoingMessage::text(format!(
                "You haven't started today's puzzle. Type a five-letter word in our DM, or `{} play` in a server channel.",
                self.prefix
            )),
            Err(e) => return self.private_error(ctx, player, &e).await,
        };
        self.private(ctx, player, &message).await
    }

    async fn stats(&self, ctx: Ctx<'_>, player: &Player) {
        match self.service.stats(player.user_id).await {
            Ok(Some(s)) if s.games_played > 0 => self.reply(ctx.channel, &render::stats(&s)).await,
            Ok(_) => {
                let text = format!("{}, you haven't finished a puzzle yet. Run `{} play` to start!", player.display_name, self.prefix);
                self.reply(ctx.channel, &OutgoingMessage::text(text)).await
            }
            Err(e) => self.internal_error(ctx.channel, &e).await,
        }
    }

    async fn set_mode(&self, ctx: Ctx<'_>, player: &Player, mode: GameMode) {
        let text = match self.service.set_preferred_mode(player, mode).await {
            Ok(change) => {
                let mut text = format!("Your preferred mode is now **{}**.", change.preferred);
                if change.applied_to_today {
                    text.push_str(" Today's game was switched too, since you haven't guessed yet.");
                }
                if change.locked_today {
                    text.push_str(" Today's game keeps its current mode because your first guess was already accepted; the new mode applies from tomorrow.");
                }
                text
            }
            Err(e) => return self.private_error(ctx, player, &e).await,
        };
        self.private(ctx, player, &OutgoingMessage::text(text)).await
    }

    /// Manual re-post of today's result (results are normally posted automatically).
    async fn share(&self, ctx: Ctx<'_>, player: &Player) {
        let Some(guild) = self.require_guild(ctx, "Sharing").await else { return };
        let text = match self.service.share(player.user_id).await {
            Ok(ShareOutcome::NoGame) => format!("You haven't played today's puzzle yet. Run `{} play`.", self.prefix),
            Ok(ShareOutcome::InProgress) => "Finish today's puzzle first; your result is posted automatically when you do.".to_string(),
            Ok(ShareOutcome::Ready(data)) => {
                let community = self.community(guild).await;
                render::share_text(&player.display_name, &data, community.as_ref())
            }
            Err(e) => return self.internal_error(ctx.channel, &e).await,
        };
        self.reply(ctx.channel, &OutgoingMessage::text(text)).await
    }

    async fn channel_setting(&self, ctx: Ctx<'_>, player: &Player, action: ChannelAction) {
        let Some(guild) = self.require_guild(ctx, "The results channel").await else { return };
        if !self.can_manage(guild, player.user_id).await {
            let text = "Only the community owner (or the bot's owner) can change the Wordle results channel.";
            return self.private(ctx, player, &OutgoingMessage::text(text)).await;
        }
        let channel_id: Option<i64> = match action {
            ChannelAction::SetHere => ctx.channel.parse().ok(),
            ChannelAction::Clear => None,
        };
        if let Err(e) = self.service.set_results_channel(guild, channel_id, player.user_id).await {
            return self.internal_error(ctx.channel, &e).await;
        }
        let text = match action {
            ChannelAction::SetHere => format!(
                "✅ Wordle results will be posted in this channel from now on. Players can start with `{} play` from any channel.",
                self.prefix
            ),
            ChannelAction::Clear => {
                "✅ Results channel cleared. Results will be posted in the channel where each player last ran `play`.".to_string()
            }
        };
        self.reply(ctx.channel, &OutgoingMessage::text(text)).await
    }

    async fn can_manage(&self, guild: i64, user: i64) -> bool {
        if self.app_owner_id == Some(user) {
            return true;
        }
        if let Some(owner) = self.guild_owners.lock().await.get(&guild).copied() {
            return owner == user;
        }
        match self.rest.get_guild(&guild.to_string()).await {
            Ok(g) => match g.owner_id.as_deref().and_then(|o| o.parse::<i64>().ok()) {
                Some(owner) => {
                    self.guild_owners.lock().await.insert(guild, owner);
                    owner == user
                }
                None => false,
            },
            Err(e) => {
                tracing::warn!(guild, error = %e, "could not look up community owner: {}", e.hint());
                false
            }
        }
    }

    /// Community streak for decorating other messages; failures are logged, not shown.
    async fn community(&self, guild_id: i64) -> Option<CommunityStreak> {
        match self.service.community_streak(guild_id).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(guild_id, error = %e, "could not load community streak");
                None
            }
        }
    }

    // ------------------------------------------------------------------ daily recap

    /// Replace past days' individual result posts with one compact recap per
    /// community and channel. Safe to call at any time; does nothing when
    /// compaction is disabled or there is nothing to do.
    pub async fn run_recaps(&self) {
        if !self.compact_results {
            return;
        }
        let _guard = self.recap_lock.lock().await;
        let pending = match self.service.pending_recaps().await {
            Ok(p) => p,
            Err(e) => return tracing::warn!(error = %e, "could not load pending recaps"),
        };
        for recap in pending {
            let results = match self.service.day_results(recap.guild_id, recap.puzzle_date).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "could not load results for recap");
                    continue;
                }
            };
            let streak = self.service.community_streak_on(recap.guild_id, recap.puzzle_date).await.ok();
            let message = render::recap(recap.puzzle_number, &results, streak.as_ref());
            let channel = recap.channel_id.to_string();
            let recap_id = match self.rest.send_message(&channel, &message).await {
                Ok(id) => id.parse::<i64>().ok(),
                // The channel is gone or the bot lost access: drop the posts rather than retry forever.
                Err(e) if matches!(e.status(), Some(403 | 404)) => {
                    tracing::warn!(%channel, error = %e, "cannot post recap; discarding: {}", e.hint());
                    None
                }
                Err(e) => {
                    tracing::warn!(%channel, error = %e, "recap failed; will retry later: {}", e.hint());
                    continue;
                }
            };
            for id in &recap.post_message_ids {
                if let Err(e) = self.rest.delete_message(&channel, &id.to_string()).await {
                    if e.status() != Some(404) {
                        tracing::warn!(%channel, message_id = id, error = %e, "could not delete result post");
                    }
                }
            }
            if let Err(e) = self.service.complete_recap(&recap, recap_id).await {
                tracing::warn!(error = %e, "could not record recap");
            }
            tracing::info!(guild = recap.guild_id, date = %recap.puzzle_date, posts = recap.post_message_ids.len(), "posted daily recap");
        }
    }

    // ------------------------------------------------------------------ sending

    async fn internal_error(&self, channel: &str, e: &Error) {
        self.reply(channel, &error_message(e)).await
    }

    async fn private_error(&self, ctx: Ctx<'_>, player: &Player, e: &Error) {
        self.private(ctx, player, &error_message(e)).await
    }

    /// Send something meant only for this player: straight back in a DM, or by DM
    /// when the command came from a server. If the DM fails, say so briefly in
    /// the channel (without the private content).
    async fn private(&self, ctx: Ctx<'_>, player: &Player, message: &OutgoingMessage) {
        if ctx.in_dm() {
            return self.reply(ctx.channel, message).await;
        }
        if let Err(e) = self.dm(player.user_id, message).await {
            tracing::warn!(user_id = player.user_id, error = %e, "could not DM player: {}", e.hint());
            let text = format!(
                "{}, I couldn't send you a DM. Allow direct messages from members of this community in your privacy settings, then try again.",
                player.display_name
            );
            self.reply(ctx.channel, &OutgoingMessage::text(text)).await
        }
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
                Ok(_) => return Ok(()),
                Err(e) if attempt == 0 && e.status() == Some(404) => {
                    self.dm_channels.lock().await.remove(&user_id);
                }
                Err(e) => return Err(e),
            }
        }
        Err(ApiError::Transport("DM channel could not be opened".into()))
    }
}

fn error_message(e: &Error) -> OutgoingMessage {
    tracing::error!(error = %e, "command failed");
    OutgoingMessage::text(match e {
        Error::PuzzleUnavailable(_) => "Today's puzzle couldn't be loaded right now. Nothing was lost; please try again in a minute.",
        _ => "Something went wrong on my side. Please try again shortly.",
    })
}
