# Twitter Post Style Guide - Furcode Features

## Tone

- Direct and technical, written by a developer, for developers.
- Confident but not hype-y. Let the feature speak for itself.
- Conversational, not corporate.

## Vocabulary

**Prefer:**
- "Furcode", "agent", "task", "context", "codebase", "workflow"
- Short, active-voice sentences.
- Concrete nouns over abstract ones ("file watcher" not "intelligent monitoring capability").

**Avoid:**
- "Forge" alone as the product name. Always use "Furcode".
- Em dashes (--) anywhere in the post. Use commas, colons, or periods instead.
- "excited to announce", "thrilled", "proud to share"
- "game changer", "revolutionary", "supercharge", "unlock", "seamlessly"
- Passive voice ("it can be used to...")
- Jargon that non-Rust developers won't know (unless the feature is Rust-specific)

## Structure Template

```
[Problem statement or developer benefit, 1 sentence]
[What the feature does / how it works, 1 sentence]
[Optional: when to use it or a concrete example, 1 sentence]

#Furcode #[FeatureTag] #AICode
```

## Approved Hashtags

Always end with `#Furcode`. Add 1-2 from the list below that best fit:

- `#AICode` - general AI-assisted coding posts
- `#DevTools` - tooling and workflow improvements
- `#RustLang` - Rust-specific features
- `#CLI` - command-line interface features
- `#CodeReview` - review and diff-related features
- `#Agents` - agent orchestration features
- `#ContextWindow` - context management features
- `#Autocomplete` - code completion features

## Example Posts

**Custom agents:**
> Furcode lets you define custom agents for specific tasks: code review, refactoring, docs. Each agent gets its own system prompt and tool set. Less context noise, better results.
>
> #Furcode #Agents #DevTools

**Shell integration:**
> Furcode's shell plugin tracks your terminal history and feeds relevant context to the agent. No more copy-pasting commands to explain what went wrong.
>
> #Furcode #CLI #DevTools

**Multi-file edits:**
> Furcode can plan and apply changes across multiple files in a single task. Rename a type, update all call sites, fix the tests, done in one pass.
>
> #Furcode #AICode #DevTools

**Context compaction:**
> Long tasks no longer blow up the context window. Furcode automatically compacts older turns while keeping the essential state. Tasks that used to fail mid-way now run to completion.
>
> #Furcode #ContextWindow #AICode

## Checklist Before Finalizing

- [ ] 2-3 sentences, fits ~280 characters
- [ ] No banned phrases
- [ ] No em dashes
- [ ] Product is referred to as "Furcode" throughout
- [ ] Leads with benefit or problem, not feature name
- [ ] Does not reference the attached video
- [ ] Ends with `#Furcode` and 1-2 relevant hashtags
