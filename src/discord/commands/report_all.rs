// Part of the Carbon Language project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::discord::{DiscordContext, DiscordError};
use crate::model;

/// Report for all users immediately, ignoring their notification times.
#[poise::command(
    slash_command,
    guild_only,
    default_member_permissions = "ADMINISTRATOR"
)]
pub async fn report_all(ctx: DiscordContext<'_>) -> Result<(), DiscordError> {
    let guild_id: model::DiscordGuildId = ctx.guild_id().unwrap().into();
    crate::discord::tasks::watch_github_report_guild(guild_id).await?;

    ctx.send(
        poise::CreateReply::default()
            .content(":saluting_face:")
            .ephemeral(true),
    )
    .await?;
    Ok(())
}
