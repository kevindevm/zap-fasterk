# Mention everyone support requirement

Upstream ZapFast PR #418 adds a composer suggestion and directly encodes bit 1
of `ContextInfo.non_jid_mentions` while rewriting localized tokens to `@all`.
This has not been adopted as a sending capability in Zap Fasterk.

The inspected whatsapp-rust revision (`94c53e833805c58c9f5966b08d08c990650a651e`, from oxidezap/whatsapp-rust)
exposes generated experiment flags for `enable_mention_everyone_sender_web`,
`enable_mention_everyone_receiver_web`, and
`admin_only_mention_everyone_group_size`, but has no documented sender helper or
feature implementation for mentioning everyone. Generated protocol fields and
flags alone do not establish a supported capability. The repository requires
protocol implementation in whatsapp-rust, rather than reimplementation here.

Before enabling the composer action, whatsapp-rust needs a supported helper
that creates the mention context, advertises current sender support, and applies
the server's current group membership/admin threshold. It must also describe
whether edits preserve the mention and how forwards and quotes avoid notifying
the destination again. Zap Fasterk can then adapt PR #418's localized composer
suggestion, rendering, permission checks, and archive tests against that API.

Typing `@all` currently remains ordinary message text. It must not be presented
as a guaranteed notification to every member.

Sources:

- https://github.com/crmne/zapfast/pull/418
- https://github.com/oxidezap/whatsapp-rust/blob/94c53e833805c58c9f5966b08d08c990650a651e/wacore/src/iq/abprops.rs
- https://github.com/oxidezap/whatsapp-rust/tree/94c53e833805c58c9f5966b08d08c990650a651e/src/features
