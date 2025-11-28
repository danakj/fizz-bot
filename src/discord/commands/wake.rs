// Part of the Carbon Language project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::discord::{DiscordContext, DiscordError};
use crate::model;

/// Generate a fresh report for yourself on active PRs immediately.
#[poise::command(slash_command, guild_only)]
pub async fn wake(ctx: DiscordContext<'_>) -> Result<(), DiscordError> {
    let guild_id: model::DiscordGuildId = ctx.guild_id().unwrap().into();
    let user_id: model::DiscordUserId = ctx.author().into();
    crate::discord::tasks::watch_github_report_user(guild_id, user_id).await?;

    ctx.send(
        poise::CreateReply::default()
            .content(":yawning_face:")
            .ephemeral(true),
    )
    .await?;
    Ok(())
}
