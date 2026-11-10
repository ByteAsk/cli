#!/usr/bin/env python3
"""Reasoning-effort mapping shared by the provider translators (stdlib only).

The engine speaks the OpenAI Responses vocabulary for reasoning effort
(``none|minimal|low|medium|high|xhigh|max|ultra``, plus an unknown ``Custom(s)``
the client may not know yet -- ``protocol/src/openai_models.rs``). No provider
accepts all of those, and each names its knob differently:

    Responses body                    Anthropic                  Gemini
    {"reasoning":{"effort":"..."}} -> output_config.effort   -> generationConfig
                                      + thinking:{adaptive}     .thinkingConfig.thinkingLevel

What is genuinely shared is only the lookup-and-fallback: read the effort out of
a Responses body, map it through a per-provider table, and fall back safely when
the value is unknown. That is what lives here. The tables themselves, and the
per-provider extras (Anthropic's thinking axis), stay next to their translator --
the vocabularies have nothing to do with each other and a merged table would
immediately leak.

⚠️ We advertise only levels the provider natively accepts (the served model
catalog lists them), so in normal traffic the table is an identity
map. The clamp still matters because ``model_reasoning_effort`` is PERSISTED
GLOBAL client config, not per-model: a user sitting at ``xhigh`` on a gpt model
carries ``xhigh`` onto Claude on their next turn without touching the picker
(verified: ``core/src/session/turn_context.rs`` writes it back on every switch).
So an out-of-range value is a normal event, not a corrupted config.
"""

__all__ = ["effort_of", "clamp_effort"]


def effort_of(body):
    """The requested reasoning effort in a Responses body, or None.

    Tolerates the whole field being absent, null, or a non-dict -- the engine
    omits ``reasoning`` entirely for any model whose catalog entry has
    ``supports_reasoning_summaries: false``, which is every third-party model
    until this feature lands.
    """
    if not isinstance(body, dict):
        return None
    reasoning = body.get("reasoning")
    if not isinstance(reasoning, dict):
        return None
    effort = reasoning.get("effort")
    if not isinstance(effort, str):
        return None
    effort = effort.strip().lower()
    return effort or None


def clamp_effort(body, table):
    """Map a Responses body's effort through ``table`` -> provider value or None.

    None means "send nothing and let the provider apply its own default" -- the
    fail-open posture every other knob in these translators uses. An unknown
    effort (a future engine level, a hand-edited config) maps to None rather
    than guessing a rung, because guessing WRONG is a silent cost or quality
    change and sending nothing is merely the status quo.
    """
    effort = effort_of(body)
    if effort is None:
        return None
    return table.get(effort)
