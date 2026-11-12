---
name: Something is not working
about: The updater did not install, did not update, or did something you did not expect
labels: bug
---

**What happened, and what did you expect instead?**


**What the updater says about itself**

Double-click `starstrings-updater.exe` again and press Ctrl+C in the window that appears: that copies what it says. Or run this in PowerShell and paste the output:

`& "$env:LOCALAPPDATA\StarStringsUpdater\starstrings-updater.exe" status | Out-Host`

```
(paste here)
```

**Last lines of the log**

The file is `%LOCALAPPDATA%\StarStringsUpdater\updater.log`.

```
(paste here)
```

**Windows version** (10 or 11):

**Anything unusual about your setup?** (game on another drive, PTU enabled, antivirus other than the built-in one, ...)


<!-- The paths above contain your Windows user name. Replace it with something else if you would rather not share it. -->
