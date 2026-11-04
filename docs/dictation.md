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

## Session words

Each recording favours words from its session that a speech model would otherwise misspell, so
they come out as the session writes them. They are the identifiers, file names and names in the
transcript (such as `DictationOptions`, `Cargo.toml` or `gpt-6`), the workspace's folder and package
names and its top-level files, and the session's models, skills and worker profiles. Recent and
repeated words count most, up to 300 of them. Words that can't be said, such as keys, hashes and
IDs, are always left out. The recording starts at once, and the words apply as soon as they're
ready. A word said differently from how it's written, such as "engine x" for nginx, still comes out
as the model hears it.

## Cleanup

Cleanup takes hesitations, repeats and false starts out of what you dictate in English. Set
`cleanup` in `dictation.json`, or Cleanup in Arko Studio's Settings › Input.

| Level | What goes |
|---|---|
| `off` | Nothing, the default |
| `light` | Hesitations such as um and uh, and stutters such as "the the" |
| `medium` | Also repeats, false starts and openers such as "So" or "I mean" |
| `full` | The same as `medium`, caught more often, with a 158 MB model |

With `medium` and `full`, what you say stays dimmed until you pause, then goes into the box
cleaned. The model of `full` goes in `models/cleanup/disfluency-tagger/model.pack` in Arko's
folder, and stays in memory beside the speech model for as long.

Cleanup only deletes words, and the session's words always stay. So do the words dictation needs
that look like fillers or repeats, such as a spoken symbol and the words beside it ("dash dash
force"), a unit after a number ("5 mm"), a filler talked about as a word ("strip the ums") and
list items that share their first word ("better tests, better docs"). Cleanup applies when the
language is English or the system names none.

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
  "download": "ask",
  "session_words": true
}
```

| Field | Values |
|---|---|
| `model` | `automatic` (by the language), `granite` or `parakeet` |
| `language` | The language you speak, such as `fr` or `pt-BR`, in place of the system's |
| `idle_minutes` | How long the model stays in memory after a recording |
| `download` | `ask` before downloading a missing model, or `automatic` |
| `session_words` | `true` to favour the [session's words](#session-words), or `false` |
| `leave_out_secrets` | `true` to leave secrets out of the session's words, `false` by default |
| `cleanup` | `off`, `light`, `medium` or `full`, for what [cleanup](#cleanup) takes out |

`leave_out_secrets` leaves out the values that context marks as secret and keeps the names. Those
are what follows a password, key or token name (`API_KEY=…`, `password: …`, `--token …`), an
`Authorization` or `Cookie` header, `Bearer`, `sshpass -p`, `mysql -p` and `curl -u`, along with
emails, the user in a URL and PEM blocks. A secret with nothing marking it, such as a bare
`Cookies$123`, can't be told from any other word.

## Building with dictation

Dictation is in builds with the `dictation` feature.

```bash
cargo install --path . --features dictation
```

It needs Rust 1.97 or later, and on Linux the microphone needs ALSA's headers (`libasound2-dev`
on Debian and Ubuntu, `alsa-lib-devel` on Fedora). `ARKO_DICTATION_WAV=clip.wav` records from a
WAV file, played in real time, in place of the microphone.

An app opens a dictation with `arko::dictation::DictationService::shared`, giving it a `Sink` that
takes the events to the recording client, and hands it audio with `Dictation::push` or
`Dictation::record`. It keeps the `Dictation` until `Done` or `Failed` comes, since dropping it
cancels the dictation. `arko::dictation::favour_session_words` gives the dictation the session's
words once they're built, on a thread of their own.
