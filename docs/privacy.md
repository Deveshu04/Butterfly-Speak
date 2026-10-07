# Privacy

The privacy policy that applies to Butterfly Speak is
[deveshu04.github.io/privacy.html](https://deveshu04.github.io/privacy.html). This page is a short
guide to what the app sends where.

- **Cloud:** your audio, your Dictionary words (as spelling hints) and the text for AI Polish,
  transforms, the voice agent, note actions, Auto-title and the prompt tester go through the
  Butterfly Labs relay on Cloudflare to Sarvam AI. Butterfly Labs keeps your Google sign-in
  details, your sign-in sessions and weekly usage counts, not your audio or text. See
  [cloud-mode.md](cloud-mode.md).
- **Bring your own key:** the same audio and text go straight to Sarvam AI under your key.
  Nothing reaches Butterfly Labs.
- **On-device:** speech recognition and AI Polish run on your computer.
- **In every mode:** your own AI endpoint, if you switch it on, receives what you switch it on
  for. With a Sarvam key saved, importing a recording and the translate shortcut send to Sarvam AI.
  The app checks GitHub for updates (you can turn this off) and downloads models from GitHub and
  Hugging Face when you ask. There is no analytics or telemetry.
- **On your computer:** settings, history, notes, models and logs (which hold no dictation text)
  live in the `ButterflySpeak` folders under `%APPDATA%` and `%LOCALAPPDATA%`; keys and the Cloud
  sign-in live in Windows Credential Manager.

## For contributors

A change to what leaves the computer, where it goes, or what is stored (on the computer, in the
relay or in Supabase) must update the [privacy policy page](https://deveshu04.github.io/privacy.html)
and the [Privacy section of the README](../README.md#privacy) in the same pull request.
