# Which AI does what

Errand ships with no AI of its own. It uses whichever ones you give it, and the
**AI** screen in the app shows, at any moment, which model would do each job and
whether your task text leaves the machine.

## Four jobs, not one

| Job | What it does | Needs |
|---|---|---|
| Doing the task | Opens the browser, signs in, decides what to click | A model that can use tools |
| Working out why something failed | Reads a failed run, suggests what to try | Any model |
| Writing the message you get | Turns an outcome into a sentence | Any model |
| Writing down what it learned | Writes the plan when a run leaves none | Any model |

Only the first has to drive a browser over many turns, and the one thing that
asks of a model is tool calling. Errand owns the loop, the tools, the budget and
the fence, whether the work is done by the Claude command line tool or by a
model on the machine under your desk, so anything that will call a tool can do
this job. Being able to is not the same as being good at it: a small model
misreads a page and gives up half way through a booking.

The other jobs are one question with one answer, so any competent model can do
them, including one on your own machine, where your run history never leaves
the house.

The plan is normally written by the agent itself at the end of a run, because it
is the only thing that watched the run happen: it knows *why* it clicked what
it clicked, which is the difference between an intent and a hint.

The fourth job is the fallback for when it does not. An agent that simply forgets
used to leave nothing at all, and a task with no plan can never be armed, so the
only remedy was to teach it again and hope. Now the plan is worked out from the
record of the run instead, by whichever model you choose. It is labelled as
inferred, and it arrives unapproved like any other, and you still read it first.

## The default, and one task at a time

The AI screen holds the default: whichever model is chosen for *doing the task*
is what every task uses. A single task can name a model of its own on its page,
and that choice wins for that task.

That is not a preference. Whichever model carries a task out is the model that
reads whatever the task reads, so a task that opens your mail wants one on your
own machine, while a task that books a tennis court may as well use the best one
you have. Bound globally, choosing a local model for the mail would put every
other task on it too.

A task's choice is a preference in one direction only: if the model it names is
switched off, removed, or put out of reach by *keep everything on this machine*,
the run falls through to the next model that works rather than failing, and says
in its journal what it was asked for and why it could not have it.

## Services

Errand knows the addresses of these already: pick one by name, paste a key, and
it works.

OpenAI · Google Gemini · OpenRouter · xAI (Grok) · DeepSeek · Moonshot (Kimi) ·
Mistral · Groq · Z.ai (GLM) · Together · Fireworks · Cerebras · Perplexity

They all speak the OpenAI chat format, which is the one thing the industry
agreed on, so a single adapter reaches every one of them and adding another is a
row in a list. Anything else that speaks that format can be added by address.

Anthropic is separate, because Errand talks to it in its own language rather
than the common one. You do not need a key: the Claude command line tool you are
already signed in to is the default.

Keys go into your macOS keychain, one entry per service, deleted with the
service. They never reach Errand's database, its logs, or the app window.

## Models on your own machines

Ollama, LM Studio, vLLM, llama.cpp, GPT4All, KoboldCpp, Jan, LiteLLM, Xinference
and Open WebUI all speak the same format. **Look for models** tries twenty ports
on this machine; ticking *look on my network too* tries all twenty on every
address of the subnet this Mac is on, which takes a few seconds.

It says afterwards how many addresses and ports it tried, and lists anything
that answered but could not be used, such as a server that wants an API key, or one
with no model loaded. Those are worth seeing: a scan that quietly drops them
looks identical to a network with nothing on it.

**What no scan can find** is a server reached by a *name* rather than a number.
Anything behind a reverse proxy that routes on the hostname (an Olares app, a
Tailscale name, a machine with its own domain) answers nothing useful at its
bare address. Add those with **Add one by address instead**.

The network sweep is off by default and only ever covers your own subnet.
Scanning a network you do not own is rude, and on a work network it looks like
something worse.

Nothing found is switched on for you. Finding a model and deciding to trust it
are separate decisions, and the second one is yours.

A machine name usually beats a number, because numbers change:
`http://mini.local:11434`. On your own machine or network you can leave off the
`/v1` and Errand adds it.

## Keeping everything local

*Keep everything on this machine* is a real restriction, not a preference: with
it on, Errand refuses to send anything to a model it does not reach on your own
machine or network. Tasks that need a browser stop working, because that needs
Claude. Errand will not let you turn it on until there is a local model to use.

## What leaves your machine

When a hosted model is used, the wording of your task and what the agent reads
on a page go to that service to be answered. Your saved logins never do: they
are typed into pages by the browser process, and the agent never receives them.
