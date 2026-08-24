# Memory commands

Local RAG over journals, newsletter digests, personal references, and tasks.

---

## `/memory <question>`

```text
/memory what was that pasta recipe I saved?
/memory when is the dentist appointment task?
```

**Flow**

1. Embed query with Ollama (`OLLAMA_EMBED_MODEL`, default `nomic-embed-text`)
2. Retrieve from the local index under `CHOTU_BRAIN_DIR` (+ task rows)
3. Answer with local `OLLAMA_MODEL`; Gemini only if Ollama fails

**Looks like**

```text
🧠 Searching memory...
<short answer with grounding from retrieved snippets>
```

## Privacy

- **Household / unlinked chat:** searches the full local index.
- **Linked personal DM:** only chunks owned by that member, plus unassigned tasks (same unassigned rule as `/tasks complete all`). Journals, digests, and personal references with no `member:` / `member_id:` in frontmatter stay household-only.
- Reflections saved from a linked DM stamp `member: <id>` in the journal YAML so they stay searchable in that DM after index.

`/memory reindex` rebuilds the whole index from any allowed chat; it does not return other people’s snippets.

---

## `/memory reindex`

Rebuilds the embedding index after bulk journal/digest changes or a model swap.

```text
/memory reindex
```

Expect a “reindexing…” style confirmation; can take a bit depending on corpus size.

---

## Needs

| Piece | Why |
| :--- | :--- |
| Ollama + embed model | Query + index |
| `OLLAMA_MODEL` | Answer generation |
| Populated brain dir / streamer digests / reflections | Something to retrieve |

No OpenRouter key required. Keep sensitive journals local; don’t commit `~/chotu_brain`.
