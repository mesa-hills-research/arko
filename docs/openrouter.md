# OpenRouter

`openrouter:<slug>` names any model OpenRouter serves, such as `openrouter:moonshotai/kimi-k3`.
Arko keeps the models you pick and lists them in `/model`'s OpenRouter group, with their context
size and prices. The row at the end of the group opens every model OpenRouter lists that takes tool
calls, which the agents need. Type to filter by name or slug, and Enter chooses. `/model openrouter:`
and a few letters completes slugs from the same list.

OpenRouter's list is read without a key, kept in memory for ten minutes and fetched again when it is
older. It never delays a request to a model.

## Settings

`/openrouter` opens a menu in the full-screen view. Anywhere else it says the settings, and these
words change them.

| Command | Setting |
|---|---|
| `/openrouter zdr on\|off` | Only providers that keep no data. Fewer models offer it, and the list shows only those. |
| `/openrouter data-collection allow\|deny` | Only providers that don't collect your prompts. Zero data retention covers it. |
| `/openrouter region any\|us\|eu\|us-or-eu` | Keeps requests in a region. US or EU sends each model to the US when OpenRouter offers it there, and to the EU otherwise. |
| `/openrouter sort default\|price\|throughput\|latency` | How OpenRouter orders the providers. |
| `/openrouter precision any\|fp8\|bf16` | The least precise weights a provider may serve. |
| `/openrouter only SLUGS\|none` | The only providers that may serve a request, such as `anthropic, google-vertex`. |
| `/openrouter ignore SLUGS\|none` | Providers that never serve a request. |
| `/openrouter refresh` | Fetches the list of models again. |

A region needs OpenRouter's Business or Enterprise plan, and fewer models are offered in one. The
privacy settings of your OpenRouter account apply too, and these settings only tighten them.

Open sessions use a change from their next request, in the terminal and in an app. Another Arko
process reads it when its next session starts. A session that can't read the file doesn't start on
an OpenRouter model, and a folder on a file system that can't lock files can't have the settings
changed.

## The file

The settings are `openrouter.json` in Arko's folder.

```json
{
  "region": "us-or-eu",
  "provider": {
    "zdr": true,
    "sort": "throughput",
    "only": ["anthropic", "google-vertex"],
    "max_price": { "prompt": 3 }
  },
  "recent": [
    { "slug": "moonshotai/kimi-k3", "label": "Kimi K3", "context": 262144, "input_price": 0.6, "output_price": 2.5 },
    "z-ai/glm-5"
  ]
}
```

`provider` is OpenRouter's [provider routing object](https://openrouter.ai/docs/guides/routing/provider-selection).
The menu and the words change `zdr`, `data_collection`, `sort`, `quantizations`, `only` and `ignore`.
Other fields, such as `max_price` or `allow_fallbacks`, work when written by hand, and Arko keeps
them. `recent` lists the models picked, newest first and at most eight, with what OpenRouter said of
each when it was picked. A slug alone is enough.
