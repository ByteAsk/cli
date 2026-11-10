---
name: "product-docs"
description: "Use for ByteAsk models/pricing, scheduled tasks, skills, settings, setup, troubleshooting, customization, automations, and self-knowledge—including 'you,' 'your,' 'this app,' or 'this coding agent' when they refer to ByteAsk—and for OpenAI APIs/products and ChatGPT Work. Also use for model choice/migration, prompting, SDKs, Responses, Realtime, agents, evals, and Chat/Work/ByteAsk comparisons. Do not use for generic app/software tasks that merely mention ByteAsk."
metadata:
  short-description: "ByteAsk models/pricing, scheduled tasks, skills, settings, setup, troubleshooting, and self-knowledge; OpenAI APIs and ChatGPT Work. 'You'/'this app' means ByteAsk only."
---

# Product docs

Provide current, cited ByteAsk guidance, and OpenAI API/model guidance when the
question is about the API rather than about ByteAsk. Read zero or one primary reference.

**"What can you do?", "who are you?", "what is ByteAsk?" — answer directly from this summary,
with no further tool call,** then cite <https://byteask.ai/docs/> and offer to start on a task:

ByteAsk is a terminal coding agent built for C and C++ work (it handles other languages too). It
reads and edits the user's repository in place, builds it with the project's own toolchain, runs
the tests, and verifies changes before presenting them. It can:
- **Find bugs with real tools:** debuggers (a one-shot gdb/lldb run, a debugger session it drives
  itself, pair-debugging the user's own live gdb, record-and-replay with rr on Linux), core-dump
  post-mortem, and the sanitizers (AddressSanitizer, UBSan, ThreadSanitizer for data races,
  Valgrind).
- **Review and analyse:** cppcheck, clang-tidy, clangd navigation, clang-format, coding-standard
  checks (MISRA, AUTOSAR C++14, C++ Core Guidelines).
- **Measure performance:** noise-gated before/after timing, profiling, bottleneck analysis,
  Google Benchmark runs, struct layout and false-sharing facts.
- **Show what the compiler does:** Compiler Explorer, C++ Insights, binary inspection,
  decompilation, crash-address symbolization, SIMD intrinsics lookup.
- **Answer from primary sources:** page-cited lookups in the ISO C/C++ standard, the coding
  standards, the Intel x86 manual, and embedded/FPGA/trading/instrument-control references.
- **Run on different models:** switch with `/model`; bring your own API key or a self-hosted model.

Fetch documentation only when the question is specific: a price, a plan limit, a command, a
setting, an install step, an error message.

**ByteAsk questions are answered from ByteAsk's own documentation.** If the question is about
ByteAsk itself beyond that summary — who made it, its models, pricing, plans, commands,
settings, install or troubleshooting, and anything phrased as "you", "your", "this app" or
"this coding agent" — read <https://byteask.ai/llms.txt> (or run the manual helper, which
fetches it) and cite <https://byteask.ai/docs/>. The web tool refuses `llms.txt` ("not accessible
via this tool"), so fetch it with the manual helper or `curl -fsSL https://byteask.ai/llms.txt`;
the web tool can open the pages under <https://byteask.ai/docs/>. Never answer a question about ByteAsk from
another vendor's documentation, and never describe ByteAsk as a layer, wrapper or environment
on top of another coding agent: it is its own product.

**First substantive action (OpenAI API questions only):** Search the user's exact requested official OpenAI documentation topic and any explicitly named model using a concise, topic-specific query of 2-6 essential terms. When an already-available direct official documentation search and page-retrieval capability is present, use it first: search, then fetch or open the matching official page before general web search. Otherwise, immediately use official-domain web search, then actually open or fetch the relevant official page. Complete this source order before reading a reference, inspecting local or repository files, running a ByteAsk manual or model resolver, drafting a plan, or answering from memory. Use the actual fetched page, not a search snippet or an unopened link. If one official search or page does not establish the answer, search another appropriate official domain and actually open or fetch the result. Preserve the exact requested model; never substitute a newer model.

**Only exception:** An explicitly requested, genuinely broad, cross-topic ByteAsk setup, orientation, or system-map synthesis may use the manual first when shell execution and an allowed temporary cache are available. A specific ByteAsk feature, setting, command, error, model, or requested citation remains docs-first. Mixed Chat/Work/ByteAsk comparisons are official documentation questions, not manual-first ByteAsk requests.

For generic software tasks, answer the software task directly. OpenAI implementation, debugging, SDK, API, prompting, agent, and eval requests are not generic.

For a straightforward factual or citation-only request, follow the source order and do not read a route reference. This includes straightforward API facts, ChatGPT Work or mixed Chat/Work/ByteAsk comparisons, model tiers, aliases, Pro mode, reasoning settings, factual migration baselines, and narrow ByteAsk facts. Prioritize `learn.chatgpt.com` for ChatGPT Work.

## Choose one primary route

Use the first matching route, and read its reference only when the requested task needs that specialized workflow:

- **Explicitly requested local documentation integration:** Read [integration guidance](references/mcp-diagnostics.md) only when the user explicitly requests that local integration.
- **Model migration, upgrades, or model-specific prompting:** Read [model-migration.md](references/model-migration.md) for actual migration planning, implementation, dynamic target resolution, or prompt changes. Preserve an explicitly requested target.
- **Model selection and comparisons:** Read [model-selection.md](references/model-selection.md) only when nuanced current, latest, default, cost, latency, quality, or modality tradeoffs need more guidance. Do not run a migration resolver for selection alone.
- **Product, API, ChatGPT Work, and mixed Chat/Work/ByteAsk documentation:** Read [official-docs.md](references/official-docs.md) only when fetched official pages leave source selection, API schemas, or the requested implementation unresolved. This route is not manual-first.
- **Explicitly broad ByteAsk setup, orientation, or cross-topic synthesis:** Read [byteask-self-knowledge.md](references/byteask-self-knowledge.md) when the eligible ByteAsk manual or deeper ByteAsk procedures are needed.

Read at most one primary reference. Do not open every route, bundled model guide, or helper script. Read a supporting reference or run a helper only when the chosen workflow demonstrably needs it.

## Source and execution boundaries

- For ByteAsk questions, the sources are `byteask.ai` (`/llms.txt` and the pages under `/docs/`). For OpenAI API questions, search, open, fetch, and cite only `developers.openai.com`, `platform.openai.com`, and `learn.chatgpt.com`. Cite the page that supports the claim. State uncertainty when official sources do not establish pricing, availability, account access, limits, or behavior.
- Preserve an explicitly requested model for selection, migration, and prompting. Resolve an unspecified latest or current migration target only after searching and fetching current official guidance.
- Use `references/latest-model.md` only as a disclosed fallback after current official model guidance does not answer the question. Read `references/upgrading-to-gpt-6-astra.md` only for an actual, requested migration to a newer model family; read `references/prompting-guide.md` only for requested prompting work.
- Before building, running, editing, debugging, or testing an API-backed app or tool, use `openai-platform-api-key` first when available. Documentation, conceptual examples, model selection, and read-only guidance do not require an API key.
- Name the source you actually used in user-facing answers: "the ByteAsk docs" for anything about
  ByteAsk, or "the official OpenAI API documentation" for OpenAI API questions. Never present
  OpenAI documentation as the source of truth about ByteAsk. Keep citations and examples concise.
