//! Rendering a conversation: markdown, tool cards, and the turn-grouped
//! transcript. One renderer serves the main pane and the subagent chat
//! windows alike — the transcript is the product's one way of showing a
//! conversation, wherever it appears.

use myco_api as api;
use yew::prelude::*;

use crate::highlight;
use crate::state::StreamItem;

/// Markdown → HTML, with raw HTML in the source neutralized (a model must
/// not be able to inject markup into the page) and fenced code blocks
/// syntax-highlighted.
///
/// The highlighter is the only thing allowed to emit markup here, and it
/// escapes the code it is given — so the un-trusted text still cannot become
/// tags, it just gets colored on the way through.
pub(crate) fn markdown(src: &str) -> Html {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let mut events = Vec::new();
    // `Some` while inside a fence: the language and the code collected so far.
    let mut fence: Option<(String, String)> = None;
    // Tables and strikethrough are extensions the parser must be asked for —
    // without them a model's table renders as one line of pipes.
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for ev in Parser::new_ext(src, options) {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                fence = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, code)) = fence.take() {
                    let body = highlight::highlight_to_html(&code, &lang);
                    events.push(Event::Html(
                        format!("<pre class=\"code\"><code>{body}</code></pre>").into(),
                    ));
                }
            }
            Event::Text(t) if fence.is_some() => {
                if let Some((_, code)) = fence.as_mut() {
                    code.push_str(&t);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => events.push(Event::Text(t)),
            other => events.push(other),
        }
    }
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    Html::from_html_unchecked(AttrValue::from(format!("<div class=\"md\">{out}</div>")))
}

/// Pretty-printed, highlighted JSON in a `<pre>`.
/// A tool call's arguments, highlighted as YAML. Falls back to escaped plain
/// text when the syntax set has no YAML, which is the highlighter's own
/// contract — the arguments are readable either way.
pub(crate) fn yaml_block(text: &str) -> Html {
    let body = highlight::highlight_to_html(text, "yaml");
    Html::from_html_unchecked(AttrValue::from(format!(
        "<pre class=\"code yaml\">{body}</pre>"
    )))
}

/// Lines of a collapsed tool result before it is cut off. Enough to see what
/// happened, short enough that a 5,000-line build log does not bury the
/// conversation.
pub(crate) const RESULT_PREVIEW_LINES: usize = 8;

/// Lines of a collapsed call's *arguments* before they are cut off. More
/// generous than the result budget — the arguments are what you scan a
/// transcript for — but a 200-line heredoc is still a click away, not a
/// wall.
pub(crate) const ARGS_PREVIEW_LINES: usize = 15;

#[derive(Properties, PartialEq)]
pub(crate) struct ToolCardProps {
    tool: api::ToolUse,
    /// The matching result, once the tool has finished.
    result: Option<api::ToolResult>,
    /// The transcript-wide verbose setting. Flipping it re-seeds every card,
    /// but a card the reader has opened by hand keeps its own state until
    /// then.
    verbose: bool,
}

/// How a call is going, as one glance.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ToolStatus {
    Running,
    Ok,
    Failed,
}

impl ToolStatus {
    fn of(result: Option<&api::ToolResult>) -> Self {
        match result {
            None => ToolStatus::Running,
            Some(r) if r.is_error => ToolStatus::Failed,
            Some(_) => ToolStatus::Ok,
        }
    }

    fn disc_class(self) -> &'static str {
        match self {
            ToolStatus::Running => "tool-disc tool-disc-running",
            ToolStatus::Ok => "tool-disc tool-disc-ok",
            ToolStatus::Failed => "tool-disc tool-disc-failed",
        }
    }

    /// Colour alone is not a label. The disc carries a title so the state is
    /// readable to a screen reader and on hover.
    fn title(self) -> &'static str {
        match self {
            ToolStatus::Running => "running",
            ToolStatus::Ok => "completed successfully",
            ToolStatus::Failed => "failed",
        }
    }
}

/// One tool call as its own bordered block: a status disc, the name, the
/// arguments, and the result folded in underneath.
///
/// The call and its output are collapsed on different budgets, because they
/// are read for different reasons. The arguments are *what was asked for* —
/// the thing you scan a transcript to find — so they get the larger cap; the
/// output is *what came back*, which can be a 5,000-line build log, so it is
/// cut sooner. One toggle opens both in full.
#[function_component(ToolCard)]
pub(crate) fn tool_card(props: &ToolCardProps) -> Html {
    let expanded = use_state(|| props.verbose);
    {
        let expanded = expanded.clone();
        use_effect_with(props.verbose, move |v| expanded.set(*v));
    }
    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_: MouseEvent| expanded.set(!*expanded))
    };
    let open = *expanded;

    // The whole call, structurally: every argument appears, long string
    // values elided. How it reads is the tool's own business — `bash` lays
    // its command out as shell, everything else falls back to the structural
    // rendering. Line-capped like the result (a 300-line heredoc command
    // would bury the conversation), with its own expander.
    let args = api::tool_display::tool_input_yaml(
        &props.tool.name,
        &props.tool.input,
        api::tool_display::TOOL_DISPLAY_WIDTH,
    );
    let args_total = args.lines().count();
    let args_shown = if open || args_total <= ARGS_PREVIEW_LINES {
        args.clone()
    } else {
        args.lines()
            .take(ARGS_PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let args_hidden = args_total.saturating_sub(ARGS_PREVIEW_LINES);
    let status = ToolStatus::of(props.result.as_ref());
    let is_error = status == ToolStatus::Failed;

    let result_body = props.result.as_ref().map(|r| {
        // Images render as images (a `view_image` preview belongs on screen,
        // not as an `[image]` note), so the text preview reads Text parts only.
        let text = text_parts(&r.content);
        let images: Vec<Html> = r
            .content
            .iter()
            .filter_map(|c| match c {
                api::Content::Image { source } => Some(render_image(source)),
                _ => None,
            })
            .collect();
        let total = text.lines().count();
        let shown = if open || total <= RESULT_PREVIEW_LINES {
            text.clone()
        } else {
            text.lines()
                .take(RESULT_PREVIEW_LINES)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let hidden = total.saturating_sub(RESULT_PREVIEW_LINES);
        html! {
            <div class="tool-result">
                { if !shown.is_empty() { html! {
                    <pre class={ if r.is_error { "err" } else { "dim" } }>{ shown }</pre>
                } } else { html!{} } }
                { for images.into_iter() }
                { if !open && hidden > 0 { html! {
                    <button class="linkish" onclick={toggle.clone()}>
                        { format!("+{hidden} more lines") }
                    </button>
                } } else { html!{} } }
            </div>
        }
    });

    // Nothing to fold away unless something got cut: a long result, or a
    // long call.
    let foldable = args_hidden > 0
        || props
            .result
            .as_ref()
            .is_some_and(|r| text_parts(&r.content).lines().count() > RESULT_PREVIEW_LINES);

    // The DOM id is the jump target for the work panel's running-tool chips.
    html! {
        <div id={format!("tool-{}", props.tool.id)}
             class={ classes!("tool-card", is_error.then_some("tool-card-error")) }>
            <div class="tool-head" onclick={toggle.clone()}>
                <span class={status.disc_class()} title={status.title()}>{ "●" }</span>
                <span class="tool-name">{ &props.tool.name }</span>
                { if foldable { html! {
                    <span class="tool-toggle">
                        { if open { "collapse" } else { "expand" } }
                    </span>
                } } else { html!{} } }
            </div>
            { yaml_block(&args_shown) }
            { if !open && args_hidden > 0 { html! {
                <button class="linkish tool-args-more" onclick={toggle.clone()}>
                    { format!("+{args_hidden} more lines of the call") }
                </button>
            } } else { html!{} } }
            { result_body.unwrap_or_else(|| html!{}) }
        </div>
    }
}

/// An image content block, inline when it is a `data:` URL — the only source
/// the tools and attachment expansion produce. Anything else stays a note: the
/// client does not fetch remote URLs on the transcript's say-so.
pub(crate) fn render_image(source: &str) -> Html {
    if source.starts_with("data:image/") {
        html! { <img class="content-image" src={source.to_string()} alt="image" /> }
    } else {
        html! { <pre class="dim">{ "[image]" }</pre> }
    }
}

/// The `Text` parts of a content run, joined — for bodies that render their
/// `Image` parts as actual images and must not also print an `[image]` note.
pub(crate) fn text_parts(content: &[api::Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            api::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A person's message, with `@handles` picked out so a room can see who is
/// being spoken to — and so you can spot your own name in a wall of text.
pub(crate) fn render_prose(text: &str, me: Option<&api::Identity>) -> Html {
    let mut out: Vec<Html> = Vec::new();
    let mut plain = String::new();
    for token in text.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        let handle = trimmed.strip_prefix('@').map(str::to_lowercase);
        let is_mention = handle
            .as_deref()
            .map(|h| !h.is_empty() && h.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)))
            .unwrap_or(false);
        if !is_mention {
            plain.push_str(token);
            continue;
        }
        if !plain.is_empty() {
            out.push(html! { { std::mem::take(&mut plain) } });
        }
        let handle = handle.unwrap_or_default();
        let handle = handle.trim_end_matches('.');
        let mine = me
            .map(|i| api::mention::matches_user(handle, &i.id, &i.name))
            .unwrap_or(false);
        let class = if mine {
            "mention mention-me"
        } else {
            "mention"
        };
        out.push(html! { <span {class}>{ trimmed }</span> });
        out.push(html! { { token.strip_prefix(trimmed).unwrap_or("") } });
    }
    if !plain.is_empty() {
        out.push(html! { { plain } });
    }
    html! { <pre>{ for out.into_iter() }</pre> }
}

/// What kind of turn a block of the transcript belongs to.
///
/// Three, by sender — the coarse split a reader actually parses at a glance.
/// Everything a model produced in one stretch is one assistant turn however
/// many content blocks it arrived in; a burst of tool activity is one tool
/// turn however many calls it contains.
#[derive(Clone, PartialEq)]
pub(crate) enum TurnKind {
    /// A person. Carries the author so two people in a row stay two turns.
    User {
        id: String,
        name: String,
    },
    Assistant,
    Tool,
}

impl TurnKind {
    /// Turns merge when this matches: consecutive entries from one sender are
    /// one turn, and a change of sender starts the next.
    fn key(&self) -> String {
        match self {
            TurnKind::User { id, .. } => format!("user:{id}"),
            TurnKind::Assistant => "assistant".into(),
            TurnKind::Tool => "tool".into(),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            TurnKind::User { .. } => "turn turn-user",
            TurnKind::Assistant => "turn turn-assistant",
            TurnKind::Tool => "turn turn-tool",
        }
    }

    fn label(&self) -> String {
        match self {
            TurnKind::User { name, .. } => name.to_uppercase(),
            TurnKind::Assistant => "ASSISTANT".into(),
            TurnKind::Tool => "TOOL".into(),
        }
    }

    fn label_class(&self) -> &'static str {
        match self {
            TurnKind::User { .. } => "role role-user",
            TurnKind::Assistant => "role role-assistant-name",
            TurnKind::Tool => "role role-tool",
        }
    }
}

/// One renderable piece of the transcript, tagged with the turn it belongs to.
///
/// The intermediate that makes grouping possible: entries do not map onto
/// turns one-to-one — a single agent entry can carry prose *and* tool calls,
/// which are two different kinds of turn — so entries are flattened to blocks
/// first and grouped second.
pub(crate) struct Block {
    kind: TurnKind,
    body: Html,
}

/// The turn a person's message belongs to.
pub(crate) fn user_turn(author: &api::Author) -> TurnKind {
    match author {
        api::Author::User { id, name } => TurnKind::User {
            id: id.clone(),
            name: name.clone(),
        },
        // System notes and anything else non-human read as the runtime
        // speaking; group them together under one heading.
        other => TurnKind::User {
            id: "system".into(),
            name: other.name().into(),
        },
    }
}

/// Flatten one entry into the blocks it contributes.
///
/// `results` is the whole transcript's tool results indexed by id, so a call
/// and its output render together even though they are separate entries.
pub(crate) fn blocks_for_entry(
    e: &api::Entry,
    results: &std::collections::HashMap<String, api::ToolResult>,
    verbose: bool,
    me: Option<&api::Identity>,
    out: &mut Vec<Block>,
) {
    match &e.body {
        api::EntryBody::User { content } => {
            let images: Vec<Html> = content
                .iter()
                .filter_map(|c| match c {
                    api::Content::Image { source } => Some(render_image(source)),
                    _ => None,
                })
                .collect();
            out.push(Block {
                kind: user_turn(&e.author),
                body: html! { <>
                    { render_prose(&text_parts(content), me) }
                    { for images.into_iter() }
                </> },
            })
        }
        api::EntryBody::Agent {
            content, tool_uses, ..
        } => {
            for c in content {
                let body = match c {
                    api::Content::Text { text } => {
                        html! { <div class="role-assistant">{ markdown(text) }</div> }
                    }
                    api::Content::Thinking { text, redacted, .. } => html! {
                        <div class="role-thinking">
                            { markdown(if *redacted { "[redacted]" } else { text }) }
                        </div>
                    },
                    api::Content::Image { source } => render_image(source),
                };
                out.push(Block {
                    kind: TurnKind::Assistant,
                    body,
                });
            }
            for t in tool_uses {
                out.push(Block {
                    kind: TurnKind::Tool,
                    // Keyed by the call id so the card that streamed is the
                    // card that persists — an in-place update, not a swap.
                    body: html! {
                        <ToolCard key={t.id.clone()} tool={t.clone()}
                                  result={results.get(&t.id).cloned()} {verbose} />
                    },
                });
            }
        }
        // Folded into the card of the call they answer.
        api::EntryBody::ToolResults { .. } => {}
    }
}

/// The whole transcript, grouped into turns.
///
/// A turn is a `<div>` around a run of blocks from one sender, headed once —
/// so one ASSISTANT heading covers a whole answer however many tool rounds it
/// took, and a run of messages from one person is headed once. There is no
/// separator element between turns: the grouping is structural, and CSS gives
/// it the space and the accent it needs.
///
/// `streaming` is the turn in flight, flattened into the same blocks as
/// anything persisted, so a reply is headed and a tool call is a card while
/// they are still arriving.
pub(crate) fn render_transcript(
    entries: &[api::Entry],
    streaming: &[StreamItem],
    arrivals: &[api::Entry],
    verbose: bool,
    me: Option<&api::Identity>,
) -> Html {
    let results = result_index(entries);
    let mut blocks: Vec<Block> = Vec::new();
    for e in entries {
        blocks_for_entry(e, &results, verbose, me, &mut blocks);
    }
    for item in streaming {
        blocks.push(stream_block(item, verbose));
    }
    // Messages delivered mid-turn come after the stream: that is where the
    // room actually saw them.
    for e in arrivals {
        blocks_for_entry(e, &results, verbose, me, &mut blocks);
    }

    // Fold the flat block list into turns.
    let mut turns: Vec<(TurnKind, Vec<Html>)> = Vec::new();
    for b in blocks {
        match turns.last_mut() {
            Some((kind, bodies)) if kind.key() == b.kind.key() => bodies.push(b.body),
            _ => turns.push((b.kind, vec![b.body])),
        }
    }

    html! {
        { for turns.into_iter().map(|(kind, bodies)| html! {
            <div class={kind.class()}>
                <div class={kind.label_class()}>{ kind.label() }</div>
                { for bodies.into_iter() }
            </div>
        }) }
    }
}

/// Index every tool result in the transcript by the call it answers.
pub(crate) fn result_index(
    entries: &[api::Entry],
) -> std::collections::HashMap<String, api::ToolResult> {
    let mut out = std::collections::HashMap::new();
    for e in entries {
        if let api::EntryBody::ToolResults { results } = &e.body {
            for r in results {
                out.insert(r.id.clone(), r.clone());
            }
        }
    }
    out
}

/// Render one streaming item as a transcript block, so live output is grouped
/// into turns by exactly the same code that groups saved output. The tool
/// card is keyed by the call's id — the identity the saved entry carries too,
/// so the card completes and then persists in place instead of shuffling.
pub(crate) fn stream_block(item: &StreamItem, verbose: bool) -> Block {
    match item {
        StreamItem::Text(text) => Block {
            kind: TurnKind::Assistant,
            body: html! { <div class="role-assistant">{ markdown(text) }</div> },
        },
        StreamItem::Thinking(text) => Block {
            kind: TurnKind::Assistant,
            body: html! { <div class="role-thinking">{ markdown(text) }</div> },
        },
        StreamItem::Tool {
            id,
            name,
            input,
            result,
        } => Block {
            kind: TurnKind::Tool,
            body: html! {
                <ToolCard key={id.clone()} tool={api::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }} result={result.clone()} {verbose} />
            },
        },
    }
}
