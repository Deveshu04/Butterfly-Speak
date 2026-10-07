<#
.SYNOPSIS
Regenerates the two speech fixtures the latency probes use, with the built-in Windows TTS voice.
utt_short.wav: one sentence (~4 s). utt_long.wav: three parts with two 1.5 s pauses (~35 s).
Run from anywhere: the files are written next to this script.
#>
Add-Type -AssemblyName System.Speech
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$s.Rate = 1
$s.SetOutputToWaveFile((Join-Path $dir "utt_short.wav"), $fmt)
$s.Speak("Could you move the standup to half past ten on Thursday? Thanks.")
$s.SetOutputToWaveFile((Join-Path $dir "utt_long.wav"), $fmt)
$ssml = '<speak version="1.0" xmlns="http://www.w3.org/2001/10/synthesis" xml:lang="en-US">Okay so for tomorrow''s meeting there are three things we need to cover. First the budget for the third quarter, second the hiring plan for the Bangalore office, and third the vendor contracts that are expiring in October.<break time="1500ms"/>I also wanted to mention that Priya sent over the revised numbers last night and they look about twenty percent higher than what we had estimated, so we should definitely flag that to finance before the call.<break time="1500ms"/>Also can someone book the big conference room on the fourth floor for two thirty to four? Please loop in Rahul from legal because the vendor stuff is going to need his sign off anyway. I think that is everything for now.</speak>'
$s.SpeakSsml($ssml)
$s.SetOutputToNull()
$s.Dispose()
Get-ChildItem (Join-Path $dir "*.wav") | ForEach-Object { "{0}  {1} bytes  ~{2:N1} s" -f $_.Name, $_.Length, (($_.Length - 44) / 32000) }
