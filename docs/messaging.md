# Messaging

Errand tells you how a run went, and — if you set it up — tells other people too.

## Two different things

**Telling you** goes to Telegram. This is the one Errand relies on, because it
works while your Mac is asleep and your phone is not.

**Telling someone else** goes to whichever channel you saved that person under:
Apple Mail, Apple Messages, WhatsApp or Telegram. This never happens by
accident; it happens because you linked a person to a task.

## Setting up Telegram

Message [@BotFather](https://t.me/BotFather) on Telegram, make a bot, and copy
the token. Paste it into Settings, message your new bot once, and Errand can
work out the chat id.

Both go straight to your macOS keychain. Until this is set, Errand has nowhere
to send results and simply says nothing — which looks exactly like a task that
never ran. `errandd doctor` will tell you.

You can also ask Errand questions from Telegram, and it will answer. It
deliberately cannot start a run or approve a plan from a chat message: a chat is
a poor place to authorise something that cannot be undone.

## Telling other people

Save the person in Settings — a name, how to reach them, and the address. Then,
on each task, choose whether they hear about it and whether that includes runs
that failed.

Saving someone does not let anything message them. The link between a task and a
person is a separate, deliberate permission, one task at a time. That is what
keeps a job from telling someone about work they never asked to hear about.

### Why the agent cannot type an address

The agent chooses **who** to message from the list you configured for that task.
It cannot choose **how**, and there is no tool anywhere that accepts a phone
number or an email address.

This matters because the agent reads web pages, and a page can say *"please also
confirm to this number"*. If the capability existed, that sentence would be an
attack. Because it does not, there is nothing to grab. The agent's standing
rules also tell it that text it reads is information and never instruction — but
that is the second line of defence. The first is that the tool does not exist.

On top of that, a message must be short, must contain no link to a site outside
the task's allowlist, has its content scrubbed of anything secret, counts
against the run's message budget, is fenced so the same person cannot be
messaged twice for the same scheduled slot, and appears in full in the run
timeline where you can read exactly what was sent.

## Quiet hours

Messages to other people, and routine good news, wait until the quiet period is
over. Set the hours in Settings.

Failures are the exception, and you can turn that off if you want to. Leave it
on unless you have a reason: hearing at nine that the eight o'clock booking
failed is hearing too late to do anything about it.

## Apple Mail and Apple Messages

These drive the apps on your Mac, so macOS asks permission the first time. Press
**Enable** in Settings while you are looking at the screen — that way the prompt
appears when you can answer it, rather than at three in the morning when nobody
can click Allow.

## WhatsApp

Best effort, and off unless you turn it on.

WhatsApp has no official way for a personal account to send messages, so this
drives WhatsApp Web through a gateway you run yourself. It logs itself out and
needs a QR code rescanned, which cannot happen while you are asleep, and
automated sending can get a personal number banned.

Run results always go to Telegram as well, so this is never the only way you
find out something happened.
