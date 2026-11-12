# Security

StarStrings Updater downloads a file from the internet and writes into a game folder with nobody watching. Reports about its safety are taken seriously.

## Reporting a problem

Please don't open a public issue for something that could put other players at risk. Report it privately instead: on this repository, open the **Security** tab and choose **Report a vulnerability**. You will get an answer there.

It helps to include the version (`starstrings-updater --version`), what you did, and what happened.

Only the latest release is supported.

## What counts

- Getting the updater to write, delete or run anything other than `.ini` files under `Data\Localization\` and its one line in `USER.cfg`.
- Getting it to accept a download over an unencrypted connection, or from somewhere it was not pointed at.
- Getting it to damage or lose a player's own files.
- Anything that lets it be used to gain rights on a PC that the person running it did not already have.

## What the updater trusts

It installs whatever the [StarStrings](https://github.com/MrKraken/StarStrings) project publishes as its release. If that project were compromised, the updater would install the result. This is why it accepts only localization text files from the download and ignores everything else in it: the worst a bad release can do is put wrong text in the game.

It also trusts the PC it runs on. Someone who can already change your files or your environment variables can redirect it, but that person does not need the updater to do harm.
