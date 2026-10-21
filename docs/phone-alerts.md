# Phone alerts

Doum can push its alerts to your phone through [ntfy](https://ntfy.sh), a free and open-source push service with
apps for iOS and Android. You don't need an account, and you can run your own server.

![Doum alerts on a phone: lock screen with a question, pauses, a resume, a failed job and GPU use outside the queue; expanded view of a question](media/phone-alerts.png)

*Illustration with Doum's real alert text. Left: alerts arriving on the lock screen. Right: the ntfy app with a
question expanded.*

## How it gets there, and how answers come back

```
 Doum (your machine)  ──HTTPS──▶  ntfy server  ──push──▶  ntfy app on your phone
        ▲                                                   │  you tap a button
        └──── Doum listens (outgoing connection) ◀── answers topic ◀─┘
```

Alerts go out from your machine. For questions, the buttons on your phone send your answer to a second private
topic, `<your topic>-answers`. Doum keeps a connection open to that topic (outgoing, like any app; nothing on your
machine is opened to the internet) and applies the answer. Each question carries a one-time code that the answer must
match, and you get a short confirmation ("Pause now for llm-writer"). If nobody answers in time, Doum applies its
recommendation and tells you what it chose.

## Setup (about two minutes)

1. **Install the ntfy app.** On iOS, get it from the App Store. On Android, use Google Play or F-Droid.

2. **Make a private topic name.** The topic works like a password: anyone who knows it can read your alerts. Make it
   long and random:

   ```sh
   echo "doum-$(openssl rand -hex 8)"      # e.g. doum-3f9c1a7e52b04d6e
   ```

3. **Subscribe in the app.** Tap **+**, enter the topic, and keep the server as `ntfy.sh`. Allow notifications when
   the phone asks.

   > **iPhone tip:** subscribe *inside the ntfy app*. If you open the topic URL (or scan a QR code) in Safari, you get
   > the ntfy web page, which only shows messages while it is open. Lock-screen alerts need the app.

4. **Tell Doum.** In `~/.config/doum/doum.toml`:

   ```toml
   [notify]
   ntfy_url = "https://ntfy.sh/doum-3f9c1a7e52b04d6e"   # your topic
   ```

   Keep this file private. Don't commit it to a public dotfiles repo.

5. **Restart the daemon** so it reads the config: `systemctl --user restart doum`. Running jobs keep running.

6. **Send yourself a test.** A job that fails on purpose triggers a "failed" alert:

   ```sh
   doum run -d -s 1 -n phone-check -- sh -c 'exit 3'
   ```

   Within a few seconds your phone shows **doum: 'phone-check' failed**, with the text *exit code 3; see its log: `doum logs N`*.

## What each alert means

| Icon | Alert | Sent when | Priority | What to do |
|---|---|---|---|---|
| ❓ | **&lt;job&gt; is waiting for the GPU** | a job does not fit and its mode is `ask` | high | Tap Smart, Wait or Pause now, or do nothing: the recommendation applies at the deadline |
| ❓ | **&lt;job&gt; has waited 20 min** | a job you are waiting on has waited `max_wait_s` | high | Pause now, Keep waiting or Cancel |
| ☑️ | **Pause now for &lt;job&gt;** / **No answer: Smart for &lt;job&gt;** | after you answered from the phone, or nobody answered in time | default | Nothing: it says what was chosen |
| ⏸️ | **Paused &lt;job&gt;** | Doum stops a job to make room for a more urgent one | default | Nothing: it saves its checkpoint and resumes on its own |
| ▶️ | **Resumed &lt;job&gt;** | a paused job starts again | default | Nothing |
| ✅ | **&lt;job&gt; finished** | a background job completes (with how long it ran) | default | Pick up the results |
| ❌ | **&lt;job&gt; failed** / **could not start** | a job exits with a non-zero code, or could not start at all | high | Look at `doum logs <id>` |
| ⏳ | **&lt;job&gt; stopped responding** | an app's reservation (such as ComfyUI's) stopped checking in for 5 minutes; its shards are released | high | Check the app; restart it if it froze |
| ⚠️ | **GPU used outside the queue** | a process uses the GPU without Doum (once per process; the smallest are ignored, see `unmanaged_min_gb`) | high | Run it with `doum run` next time so it waits its turn |

`high` alerts make a sound and show as a banner. With `default`, the ntfy app's own settings apply, so you can mute
info alerts per topic in the app.

A question on the phone (the same text the panel and the desktop pop-up show):

```
❓ llm-writer is waiting for the GPU
Needs 110 of 110 shards; 50 free.
In the way: comfyui (60 shards, done in ~12 min, rendering 3/8).

Smart: pause comfyui, start in about 2 min
Wait: pause nothing, start in about 12 min
Pause now: pause comfyui, start in about 2 min

No answer in 1 min: Smart.
[ Smart (recommended) ]  [ Wait ]  [ Pause now ]
```

All of these also stay in **Recent alerts** in the web panel (`doum panel`) and in `doum alerts`, so an alert you
swiped away is not lost.

## Privacy

- **The topic is the password.** On the public `ntfy.sh` server, anyone who knows your topic can read your alerts,
  which include job names and what is running. Use a long random topic and don't post screenshots that show it.
- **Running your own server:** ntfy is a single binary or a Docker image
  ([docs](https://docs.ntfy.sh/install/)). Point `ntfy_url` at it, for example `https://ntfy.example.com/doum`. Doum
  does not send login credentials yet, so the server must accept anonymous publishing to that topic. Keep it private
  by making it reachable only over your VPN, for example Tailscale.
- **Answers:** the answers topic is as private as the alerts topic (same name plus `-answers`), and an answer only
  counts if it carries the one-time code of a question that is still open. To keep the alerts but turn the buttons
  off, set `phone_answers = false`.
- **Turning it off:** set `ntfy_url = ""`.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Nothing arrives at all | `ntfy_url` empty or misspelled; the daemon was not restarted after editing the config; check `journalctl --user -u doum` for `alert title=...` lines |
| Alerts show only while the app is open | You subscribed in the browser, not in the app (see the iPhone tip above), or notifications are off for ntfy in the phone's settings |
| No sound for pauses and resumes | Expected: they are `default` priority; raise it per topic in the app |
| Silent during Focus or Do Not Disturb | Allow ntfy in that Focus mode, or rely on `high` alerts breaking through if you allow time-sensitive notifications |
| Too many "outside the queue" alerts | Raise `unmanaged_min_gb`, or run those tools through `doum run` |
| Buttons do nothing | Check `journalctl --user -u doum` for `listening for phone answers`; the daemon needs internet access to the ntfy server, and `phone_answers` must be on. An answer to a question that already timed out is ignored |

## Webhooks instead

For Slack, Discord, Home Assistant or your own scripts, add URLs to `webhooks`. Each alert is POSTed as JSON (see
[integrations](integrations.md#webhooks) for the payload).
