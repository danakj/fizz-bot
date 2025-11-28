// Part of the Carbon Language project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use tokio::sync::{mpsc, Mutex};

use crate::discord;
use crate::discord::{DiscordData, DiscordError};
use crate::github;
use crate::model;

const WAKE_UP_FREQ_SECONDS: u64 = 60 * 5;

static WAKE_AND_REPORT_GUILD: Mutex<Option<mpsc::Sender<model::DiscordGuildId>>> =
    Mutex::const_new(None);
static WAKE_AND_REPORT_USER: Mutex<
    Option<mpsc::Sender<(model::DiscordGuildId, model::DiscordUserId)>>,
> = Mutex::const_new(None);

pub async fn watch_github(http: Arc<serenity::Http>, data: Arc<DiscordData>) {
    let mut interval: tokio::time::Interval =
        tokio::time::interval(std::time::Duration::from_secs(WAKE_UP_FREQ_SECONDS));

    // A channel for `/report_all` to wake this task up before `interval`, asking
    // for an immediate report for all users in a guild.
    let (send_wake_and_report_guild, mut recv_wake_and_report_guild) = mpsc::channel(100);
    WAKE_AND_REPORT_GUILD
        .lock()
        .await
        .replace(send_wake_and_report_guild);

    // A channel for `/wake` to wake this task up before `interval`, asking
    // for an immediate report for a single user.
    let (send_wake_and_report_user, mut recv_wake_and_report_user) = mpsc::channel(100);
    WAKE_AND_REPORT_USER
        .lock()
        .await
        .replace(send_wake_and_report_user);

    let mut last_report_timestamp = Utc::now();
    // Loop forever, waiting for the next update period and doing the update.
    loop {
        let mut filter_guild_id = None;
        let mut filter_user_id = None;
        tokio::select! {
            guild_id = recv_wake_and_report_guild.recv() => {
                filter_guild_id = Some(guild_id.expect("WAKE_AND_REPORT_GUILD was closed"))
            }
            guild_and_user_id = recv_wake_and_report_user.recv() => {
                let (guild_id, user_id) = guild_and_user_id.expect("WAKE_AND_REPORT_GUILD was closed");
                filter_guild_id = Some(guild_id);
                filter_user_id = Some(user_id);
            }
            _ = interval.tick() => {
            }
        }

        let now = Utc::now();

        let run_result = report_alerts(
            http.clone(),
            data.clone(),
            &last_report_timestamp,
            &now,
            std::mem::replace(&mut filter_guild_id, None),
            std::mem::replace(&mut filter_user_id, None),
        )
        .await;
        match run_result {
            Ok(()) => {
                last_report_timestamp = now;
            }
            Err(e) => {
                eprintln!("ERROR: watching github {}", e);
                // Wait a bit and try again.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        }
    }
}

async fn report_alerts(
    http: Arc<serenity::Http>,
    data: Arc<DiscordData>,
    last_report_timestamp: &DateTime<Utc>,
    now: &DateTime<Utc>,
    ignore_time_for_guild_id: Option<model::DiscordGuildId>,
    filter_to_discord_user: Option<model::DiscordUserId>,
) -> Result<(), DiscordError> {
    struct GuildAlerts {
        discord_channel_id: model::DiscordChannelId,
        discord_user_ids: Vec<model::DiscordUserId>,
        prs: Arc<Vec<github::Pr>>,
        issues: Arc<Vec<github::LeadsIssue>>,
    }
    let mut alerts = Vec::new();

    struct GuildWeeklyAlerts {
        discord_guild_id: model::DiscordGuildId,
        discord_channel_id: model::DiscordChannelId,
        discord_user_ids: Vec<model::DiscordUserId>,
        issues: Arc<Vec<github::LeadsIssue>>,
    }
    let mut weekly_alerts = Vec::new();

    let guild_ignores_time = |guild_id| match &ignore_time_for_guild_id {
        Some(ignored_guild_id) => ignored_guild_id == guild_id,
        None => false,
    };

    {
        let cfg_guard = data.cfg.lock().await;
        for (guild_id, guild_config) in &cfg_guard.guilds {
            if guild_config.report_channel_id.is_empty() {
                continue;
            }

            // This is an `await` while we have a mutex guard on `data.cfg`, but
            // its okay because we don't give access to `data` here. The future
            // waits on Github access only.
            let prs_state =
                github::get_prs(&guild_config.repo_owner, &guild_config.repo_name).await?;
            let issues_state =
                github::get_leads_issues(&guild_config.repo_owner, &guild_config.repo_name).await?;

            let mut discord_user_ids_to_alert: Vec<model::DiscordUserId> = Vec::new();
            let mut discord_user_ids_to_weekly_alert: Vec<model::DiscordUserId> = Vec::new();
            for (discord_user_id, _) in &guild_config.users {
                let user_alerts = model::discord_user_report_times(guild_config, discord_user_id);

                if guild_ignores_time(guild_id) {
                    discord_user_ids_to_alert.push(discord_user_id.clone());
                    discord_user_ids_to_weekly_alert.push(discord_user_id.clone());
                } else {
                    // Look for any alert times that we have passed since the last report attempt.
                    let should_report =
                        |report_time| last_report_timestamp < report_time && now >= report_time;
                    if user_alerts.iter().any(should_report) {
                        discord_user_ids_to_alert.push(discord_user_id.clone());

                        if model::discord_user_weekly_report_needed(guild_config, discord_user_id) {
                            discord_user_ids_to_weekly_alert.push(discord_user_id.clone());
                        }
                    }
                }
            }

            if let Some(filter_user) = &filter_to_discord_user {
                discord_user_ids_to_alert.retain_mut(|user| *user == *filter_user);
                discord_user_ids_to_weekly_alert.retain_mut(|user| *user == *filter_user);
            }

            let prs: Arc<Vec<_>> =
                Arc::new(github::filter_prs_for_guild(prs_state, guild_config).collect());
            let issues: Arc<Vec<_>> = Arc::new(
                github::filter_leads_issues_for_guild(issues_state, guild_config).collect(),
            );

            alerts.push(GuildAlerts {
                discord_channel_id: guild_config.report_channel_id.clone(),
                discord_user_ids: discord_user_ids_to_alert,
                prs,
                issues: issues.clone(),
            });
            weekly_alerts.push(GuildWeeklyAlerts {
                discord_guild_id: guild_id.clone(),
                discord_channel_id: guild_config.report_channel_id.clone(),
                discord_user_ids: discord_user_ids_to_weekly_alert,
                issues,
            });
        }
    }
    // Drop the mutex guard before doing any `await` to avoid blocking other tasks.

    for alert in weekly_alerts {
        for discord_user_id in &alert.discord_user_ids {
            report_weekly_alerts_for_user(
                http.clone(),
                alert.issues.clone(),
                alert.discord_channel_id.clone(),
                discord_user_id.clone(),
            )
            .await?;

            // Reset the timestamp for weekly alerts for each user that was just given
            // their weekly alerts, but only if it was not done through a slash command
            // like `/wake`.
            if ignore_time_for_guild_id.is_none() && filter_to_discord_user.is_none() {
                let mut cfg_guard = data.cfg.lock().await;
                if let Some(guild_config) = cfg_guard.guilds.get_mut(&alert.discord_guild_id) {
                    for discord_user_id in &alert.discord_user_ids {
                        if let Some(user_config) = guild_config.users.get_mut(discord_user_id) {
                            user_config.last_weekly_report = Some(*now);
                        }
                    }
                }
            }
        }
    }
    discord::util::save_config(&data).await?;

    for alert in alerts {
        for discord_user_id in alert.discord_user_ids {
            report_alerts_for_user(
                http.clone(),
                alert.prs.clone(),
                alert.issues.clone(),
                alert.discord_channel_id.clone(),
                discord_user_id,
            )
            .await?;
        }
    }

    Ok(())
}

async fn delete_messages_with_prefix(
    http: Arc<serenity::Http>,
    discord_channel_id: model::DiscordChannelId,
    prefix: String,
) -> Result<(), DiscordError> {
    let user = http.get_current_user().await?;
    discord::util::delete_messages(http, discord_channel_id, |m| {
        if let Some(flags) = m.flags {
            if flags.contains(serenity::MessageFlags::EPHEMERAL) {
                return false;
            }
        }
        if m.author.id != user.id {
            return false;
        }

        m.content.starts_with(&prefix)
    })
    .await
}

fn format_pr(pr: &github::Pr) -> String {
    let mut msg: String = String::new();
    msg.push_str(&format!("[PR #{}](<{}>)", pr.github_pr.number, pr.url));
    if let Some(user) = &pr.github_pr.user {
        msg.push_str(&format!(" **{}**", user.login));
    }
    if let Some(title) = &pr.github_pr.title {
        msg.push_str(&format!("\n    {}", title));
        // Close unbalanced formatting characters.
        if title.chars().filter(|c| *c == '`').count() % 2 == 1 {
            msg.push('`');
        }
    }
    msg
}

fn format_issue(issue: &github::LeadsIssue) -> String {
    let mut msg = String::new();

    let title = &issue.github_issue.title;
    msg.push_str(&format!(
        "[Issue #{}](<{}>) {}",
        issue.github_issue.number, issue.url, title
    ));
    // Close unbalanced formatting characters.
    if title.chars().filter(|c| *c == '`').count() % 2 == 1 {
        msg.push('`');
    }
    msg
}

/// Send one or more messages, with each message capped at 2000 bytes (discord's limit).
async fn generate_alert_messages<'a, T, ToString: Fn(&'a T) -> String>(
    http: Arc<serenity::Http>,
    header: String,
    alerts: Vec<&'a T>,
    to_string: ToString,
    discord_channel_id: model::DiscordChannelId,
) -> Result<(), DiscordError> {
    let header_len = header.len();

    let mut i = 0;
    while i < alerts.len() {
        let mut msg = String::new();
        let mut msg_len = 0;
        loop {
            let Some(alert) = alerts.get(i) else {
                break;
            };
            let line = &format!("\n* {}", to_string(alert));
            if header_len + msg_len + line.len() > 2000 {
                break;
            }
            msg.push_str(line);
            msg_len += line.len();
            i += 1;
        }

        if !msg.is_empty() {
            let msg = serenity::CreateMessage::new().content(format!("{}{}", header, msg));
            http.send_message(discord_channel_id.clone().into(), vec![], &msg)
                .await?;
        }
    }

    Ok(())
}

async fn report_alerts_for_user(
    http: Arc<serenity::Http>,
    prs: Arc<Vec<github::Pr>>,
    issues: Arc<Vec<github::LeadsIssue>>,
    discord_channel_id: model::DiscordChannelId,
    discord_user_id: model::DiscordUserId,
) -> Result<(), DiscordError> {
    const PR_HEADER: &str = ":notepad_spiral: PRs for review ";
    const BLOCKING_ISSUES_HEADER: &str = ":fire_engine: Open leads issues (blocking) ";

    let pr_header = format!("{}{}", PR_HEADER, discord_user_id);
    let issue_header = format!("{}{}", BLOCKING_ISSUES_HEADER, discord_user_id);

    delete_messages_with_prefix(http.clone(), discord_channel_id.clone(), pr_header.clone())
        .await?;
    delete_messages_with_prefix(
        http.clone(),
        discord_channel_id.clone(),
        issue_header.clone(),
    )
    .await?;

    let user_prs: Vec<_> = prs
        .iter()
        .filter(|pr| {
            pr.reviewers
                .iter()
                .any(|r| r.discord_users.contains(&discord_user_id))
        })
        .collect();
    let user_issues: Vec<_> = issues
        .iter()
        .filter(|issue: &_| {
            issue.urgency == github::Urgency::Blocked && issue.leads.contains(&discord_user_id)
        })
        .collect();

    generate_alert_messages(
        http.clone(),
        pr_header,
        user_prs,
        format_pr,
        discord_channel_id.clone(),
    )
    .await?;
    generate_alert_messages(
        http,
        issue_header,
        user_issues,
        format_issue,
        discord_channel_id,
    )
    .await?;
    Ok(())
}

async fn report_weekly_alerts_for_user(
    http: Arc<serenity::Http>,
    issues: Arc<Vec<github::LeadsIssue>>,
    discord_channel_id: model::DiscordChannelId,
    discord_user_id: model::DiscordUserId,
) -> Result<(), DiscordError> {
    const NONURGENT_ISSUES_HEADER: &str = ":chipmunk: Open leads issues (non-blocking) ";

    let header = format!("{}{}", NONURGENT_ISSUES_HEADER, discord_user_id);

    delete_messages_with_prefix(http.clone(), discord_channel_id.clone(), header.clone()).await?;

    let user_issues: Vec<_> = issues
        .iter()
        .filter(|issue: &_| {
            issue.urgency == github::Urgency::Normal && issue.leads.contains(&discord_user_id)
        })
        .collect();

    generate_alert_messages(http, header, user_issues, format_issue, discord_channel_id).await?;
    Ok(())
}

pub async fn watch_github_report_guild(
    guild_id: model::DiscordGuildId,
) -> Result<(), DiscordError> {
    let guard = WAKE_AND_REPORT_GUILD.lock().await;
    if let Some(sender) = guard.as_ref() {
        match sender.send(guild_id).await {
            Ok(()) => {}
            Err(e) => return Err(e.to_string().into()),
        }
    }
    Ok(())
}

pub async fn watch_github_report_user(
    guild_id: model::DiscordGuildId,
    user_id: model::DiscordUserId,
) -> Result<(), DiscordError> {
    let guard = WAKE_AND_REPORT_USER.lock().await;
    if let Some(sender) = guard.as_ref() {
        match sender.send((guild_id, user_id)).await {
            Ok(()) => {}
            Err(e) => return Err(e.to_string().into()),
        }
    }
    Ok(())
}
