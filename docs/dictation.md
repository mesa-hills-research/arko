# Dictation

`/dictate` types what you say into the message box. The speech is recognized on the computer Arko
runs on, and recordings aren't kept.

## In the terminal

In the full-screen view, `/dictate` or Ctrl+R starts recording, and the status line says
listening. Words show dimmed after the cursor as you speak and go into the box as they settle.
Ctrl+R or Enter stops, the words stay in the box to edit, and Enter sends them. Esc cancels and
takes out the words the recording put in, wherever they are, and what you typed meanwhile stays.

At the line prompt (`--repl`), `/dictate` prints the words as they settle until you press Enter,
then leaves them at the prompt to edit and send.

Arko records from the microphone of the computer it runs on, so over SSH it records from the remote
computer's, and says so.

## The speech model

The first recording asks before it downloads a model from Hugging Face. Arko checks the download,
converts it to 8-bit weights and keeps it in `models/speech/` in Arko's folder, with a `NOTICE.txt`
that credits its maker. An interrupted download resumes where it stopped. While it runs, the status
line shows its progress, the message box works as usual, and Ctrl+R cancels it.

| Model | Languages | Download | On disk | Licence |
|---|---|---:|---:|---|
| [Granite Speech 5.0 470M TurboCTC](https://huggingface.co/ibm-granite/granite-speech-5.0-470m-turboctc) by IBM | English, French, German, Spanish, Portuguese, Italian, Japanese | 948 MB | 477 MB | Apache 2.0 |
| [Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) by NVIDIA | 25 European languages | 715 MB | 723 MB | CC BY 4.0 |

Arko uses Granite Speech, and Parakeet for the European languages Granite doesn't know, going by the
system's language (`LANG`). The model stays in memory while recordings follow and leaves it after
10 minutes without one. To remove a model, delete its folder.

## Settings

`dictation.json` in Arko's folder holds the settings, read at each recording. Every field may be
left out.

```json
{
  "model": "automatic",
  "language": "fr",
  "idle_minutes": 10,
  "download": "ask"
}
```

| Field | Values |
|---|---|
| `model` | `automatic` (by the language), `granite` or `parakeet` |
| `language` | The language you speak, such as `fr` or `pt-BR`, in place of the system's |
| `idle_minutes` | How long the model stays in memory after a recording |
| `download` | `ask` before downloading a missing model, or `automatic` |

## Building with dictation

Dictation is in builds with the `dictation` feature.

```bash
cargo install --path . --features dictation
```

It needs Rust 1.97 or later, and on Linux the microphone needs ALSA's headers (`libasound2-dev`
on Debian and Ubuntu, `alsa-lib-devel` on Fedora). `ARKO_DICTATION_WAV=clip.wav` records from a WAV file, played in real
time, in place of the microphone.

An app opens a dictation with `arko::dictation::DictationService::shared`, hands it audio with
`Dictation::push` or `Dictation::record`, and passes its events to `Feed::dictation`, which sends
them to every client as `Update::Dictation`.
