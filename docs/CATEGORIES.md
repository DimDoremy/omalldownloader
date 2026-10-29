# Where downloads go

The folder is chosen by the file's extension. The rules live in
`~/.config/omalldownloader/categories.json`, which is written with the defaults the first
time the program runs, so they can be edited rather than guessed at:

```json
[
  { "folder": "ISO",      "extensions": ["iso", "img", "dmg", "vhd"] },
  { "folder": "Video",    "extensions": ["mkv", "mp4", "avi", "webm", "mov", "m4v", "ts", "flv"] },
  { "folder": "Music",    "extensions": ["mp3", "flac", "m4a", "opus", "wav", "ogg"] },
  { "folder": "ABDM",     "extensions": ["abdm"] }
]
```

* the first rule whose extension matches wins; a file that matches none goes into the
  download folder itself;
* folders are created on demand, and only names are accepted — `../evil` is dropped, not
  followed;
* a name that is already taken becomes `name (1).ext`, `name (2).ext`, the way a file
  manager numbers them, so two downloads of the same file never overwrite each other.

Both the window and `omalld` use the same rules, and `categories.json` is read on every
task, so an edit applies to the next download without a restart.
