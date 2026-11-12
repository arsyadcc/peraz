# Security policy

## Supported versions

Peraz is in alpha. Security fixes go into the next release; only the latest release is
supported, so please update before reporting.

| Version              | Supported |
|----------------------|-----------|
| latest release       | yes       |
| older releases       | no        |

## Reporting a vulnerability

Please report security problems privately, not in a public issue:
use [**Report a vulnerability**](https://github.com/arsyadcc/peraz/security/advisories/new) on the
repository's Security tab (GitHub private vulnerability reporting).

Include what you can of:

- the Peraz version (`peraz --version`) and your platform;
- the FFmpeg version (`peraz doctor`);
- the steps or the file that trigger the problem (attach a sample if you can share it);
- what an attacker could achieve.

You will get an answer within a week. Once a fix is released, the advisory is published with
credit to you, unless you prefer otherwise.

## Scope

Peraz reads files that may come from anyone, so problems such as a crafted file that makes Peraz
write outside the output folder, overwrite a file it should not, or run a program are in scope.
Peraz runs FFmpeg as a separate program: vulnerabilities in FFmpeg itself belong to the
[FFmpeg project](https://ffmpeg.org/security.html), but tell us if Peraz makes one easier to
reach.
