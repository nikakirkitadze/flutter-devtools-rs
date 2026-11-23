# Security policy

## Reporting a problem

Please don't report security problems in public issues. Use GitHub's private reporting instead:
open the repository's **Security** tab and choose **Report a vulnerability**. Only the maintainer
sees the report.

Include what you found, how to reproduce it, and what an attacker could do with it. You'll get a
reply within a week.

## Supported versions

fdt has no releases yet. Fixes go into the `master` branch.

## Scope

fdt connects to Dart VM services on your machine and on attached devices, and it reads device logs
through `adb` and `xcrun simctl`. Problems in how it handles that data, such as a malicious app
making fdt run commands or read files it shouldn't, are in scope.
