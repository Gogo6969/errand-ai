/**
 * Every explanation in the app, in one place.
 *
 * The rule is that nothing goes unexplained, and a rule like that only survives
 * a growing codebase if a machine checks it. So every control is wrapped in a
 * Hint, every Hint names an entry here, and `npm run hints:audit` fails the
 * build on a control without one or an entry nobody uses.
 *
 * Write these for someone who has not read the documentation and is slightly
 * worried about what this thing is going to do to their accounts. Say what the
 * control does, and where it matters, say what it will not do.
 */

export interface Hint {
  /** One sentence, on hover and for screen readers. */
  short: string;
  /** The fuller answer, behind "Tell me more". Optional. */
  long?: string;
}

export const hints = {
  // ---------------------------------------------------------------- tasks --
  "app.retry": {
    short: "Asks Errand's background service again.",
    long:
      "The service starts by itself and is briefly unreachable after an update or when the Mac \
       wakes. This retries on its own a few times; the button is for when you would rather not \
       wait. Nothing is lost while it is unreachable: your tasks are on disk.",
  },
  "task.new": {
    short: "Describe a job in your own words. Errand works out how to do it.",
    long:
      "You write what you want done the way you would explain it to a person. Errand tries it " +
      "once while you watch, writes down what worked, and shows you that before it ever does the " +
      "job on its own.",
  },
  "task.create": {
    short: "Sets it up and does the job.",
    long:
      "Errand reads what you wrote, works out what the job needs, and does it. It cannot run on " +
      "a schedule until you have seen it work once.",
  },
  // One per state, because a single sentence covering all of them explains the
  // one the reader is looking at least of all. The pill says what the task is;
  // these say what that means and what it is waiting for.
  "task.state.draft": {
    short: "Written down, but never tried.",
    long:
      "Press Do it now and it works the job out from your description and does it. If the job " +
      "books, sends, buys or moves anything, rehearse it once first: it goes through the whole " +
      "thing with none of it really happening. Nothing runs while you are not watching until it " +
      "has really done the job once.",
  },
  "task.state.teaching": {
    short: "Trying the job for the first time, from your description alone.",
    long:
      "It has no plan yet, so it is working the job out as it goes. Open the run to watch each " +
      "step as it happens.",
  },
  "task.state.awaiting_approval": {
    short: "It finished and wrote down how it did the job.",
    long:
      "What it wrote is what it will follow next time. You do not have to approve it: it just " +
      "did the job that way and it worked. Read it under the gear if you want to change how it " +
      "goes about it.",
  },
  "task.state.teach_failed": {
    short: "It tried the job and could not finish. The run says why.",
    long:
      "Nothing is broken and nothing was lost. Open the run to see where it stopped: usually the " +
      "description needs to be more specific, a site is missing from the list, or it needed a " +
      "login it does not have.",
  },
  "task.state.running": {
    short: "Working on it now.",
    long: "Open the run to watch each step as it happens.",
  },
  "task.state.ready": {
    short: "Armed. It will run on its schedule, or whenever you ask.",
    long:
      "It has done the job at least once, so it knows how. If it has no schedule it runs only " +
      "when you ask.",
  },
  "task.state.paused": {
    short: "You paused it. Scheduled runs are skipped; nothing has been deleted.",
    long:
      "Its plan, history and logins are all kept. Resuming does not go back and do the runs it " +
      "missed; it picks up at the next one.",
  },
  "task.state.needs_attention": {
    short: "Errand paused this itself and is waiting for you.",
    long:
      "Usually a run began something that cannot be undone and stopped before confirming it. " +
      "Errand will not guess: check the site, then tell it what you found, and it carries on.",
  },
  "task.state.archived": {
    short: "Put away. It does not run and does not appear in the ordinary list.",
  },
  "task.watch_run": {
    short: "Open the run that is going on now and watch it step by step.",
    long:
      "The steps appear as they happen. If the count on this card has stopped moving, the run " +
      "itself says what it was in the middle of.",
  },
  "task.directive": {
    short: "Change what you asked for, in your own words.",
    long:
      "This text is the task. It is what the agent reads and the only thing that decides what it \
       does, so changing it is how you correct an outcome that was not what you meant.",
  },
  "task.directive_save": {
    short: "Saves the new wording and does the job again with it.",
    long:
      "Straight away, because the only thing that settles whether the new wording is better is \
       another outcome. What it learned before is kept until a run replaces it.",
  },
  "task.answer": {
    short: "Sends your answer and does the job again.",
    long:
      "The answer is kept on the run that asked, so the question and what you said sit together, " +
      "and the next run is given it. Never type a password or a card number here.",
  },
  "task.settings": {
    short: "How this task works: when it runs, what it may open, which AI, who it tells.",
    long:
      "Kept out of the way on purpose. Errand fills these in from what you wrote and you can " +
      "leave them alone; open this when you want to change the schedule, take a permission " +
      "back, or point the task at a different model.",
  },
  "task.last_run": {
    short: "Open the newest run and see everything it did.",
    long:
      "Every step it took, in order, with the screenshots it kept. If it could not finish, the " +
      "run says what it was doing when it stopped and what you can do about it.",
  },
  "task.next_run": {
    short: "When this will next run on its own.",
    long:
      "This is the moment the run actually begins, including any early start it needs to be " +
      "logged in before a booking opens, so it is the time you would see on a clock.",
  },
  "task.run_now": {
    short: "Run it now, without waiting for its schedule.",
    long:
      "Anything that cannot be undone is still protected: if this task already booked or sent " +
      "something for the same slot, running now will not do it twice.",
  },
  "task.dry_run": {
    short: "Walk through it without actually doing anything.",
    long:
      "A rehearsal. The agent goes through the task normally, but anything irreversible is " +
      "recorded as what it would have done instead of happening. Nothing is booked, sent or " +
      "deleted.",
  },
  "task.pause": {
    short: "Skip scheduled runs. Manual runs still work. Nothing is deleted.",
    long:
      "Pausing keeps the task, its plan, its history and its logins. When you resume, it does not " +
      "go back and do the runs it missed; it simply picks up at the next one.",
  },
  "task.resume": {
    short: "Start following its schedule again.",
    long:
      "It does not go back and do the runs that were skipped while paused; it simply picks up " +
      "at the next one. If Errand paused the task itself, check why before resuming: it may be " +
      "waiting for you to say what you found on a site.",
  },
  "task.activate": {
    short: "Let this run on its own schedule from now on.",
    long:
      "Only offered once it has really done the job with you there. Proven, rather than approved: " +
      "a rehearsal does not count, because a rehearsal touches nothing.",
  },
  "task.allowed_sites": {
    short: "The only websites this task may open.",
    long:
      "Anything else is refused, including a redirect from an allowed site. A task with no sites " +
      "listed cannot browse at all. This is also what stops a page that looks like your bank " +
      "from being treated as your bank. The same list decides what a task may put on your screen: " +
      "a page opened in front of you at 7am has to be on it too.",
  },
  "task.limits": {
    short: "Ceilings on how long a run may take and what it may spend.",
    long:
      "A run that goes round in circles is stopped rather than left going. If a run hits a " +
      "ceiling it says which one, so you can tell a genuinely slow site from a stuck agent.",
  },
  "task.edit_limits": {
    short: "Change what one run of this task may spend.",
    long:
      "A zero on any of them means no ceiling for that one, which is only sensible for a site " +
      "you trust to finish. The message limit also caps how many people a run can tell.",
  },
  "task.save_limits": {
    short: "Save them. They apply from the next run.",
  },
  "task.model": {
    short: "Which AI carries out this task. Saved as soon as you pick it.",
    long:
      "Whichever model does the job is the one that reads what the job reads, so a task that " +
      "opens your mail is worth putting on a model that runs on your own machine, while a task " +
      "that books a court can use anything. Leave it on Default and this task follows the " +
      "choice on the AI screen. A model Errand has found cannot drive a browser is shown here " +
      "but cannot be picked, and if the one you pick is later switched off, the task still runs " +
      "on something that works and the run says what happened.",
  },

  // ------------------------------------------------------------- playbook --
  "playbook.what": {
    short: "What the agent learned about doing this job.",
    long:
      "Written in plain markdown so you can read it, and stored as a file you could open in any " +
      "editor. Each step records what it was trying to achieve, separately from how it did it " +
      "last time, because sites move buttons and intentions do not change.",
  },
  "playbook.approve": {
    short: "Use this way of doing the job instead of the current one.",
    long:
      "The first plan is adopted by itself, because the run that wrote it had just done the job. " +
      "A later one waits for you: a plan is written from pages by strangers and then followed as " +
      "instructions, so replacing one that already works is worth a look first.",
  },

  // ------------------------------------------------------------------ run --
  "run.answer": {
    short: "What the task produced, which is the reason you set it up.",
    long:
      "Separate from what the run did. If you asked to be told something, this is the telling. " +
      "If you asked for something to be done, this is what is now true and the proof of it. " +
      "A run that could not finish still shows what it found before it stopped.",
  },
  "run.answer_copy": {
    short: "Opens the note or file where this run also put the answer.",
    long:
      "Errand makes a copy like this only when the task asked for one. The answer above is the " +
      "original and is always here, whether or not a copy could be written.",
  },
  "run.timeline": {
    short: "Everything the agent did, in order, in its own words.",
    long:
      "Each line is one thing it did or decided. Where it took a screenshot you can see the page " +
      "as it saw it, with password fields covered before the picture was taken.",
  },
  "run.status": {
    short: "How this run ended.",
  },
  "run.rehearsal": {
    short: "Nothing in this run really happened.",
    long:
      "It went through the whole job, and anything it could not have taken back was recorded as " +
      "what it would have done. Nothing was booked, sent, bought or moved, so where the steps " +
      "below say it did something, read that as what the first real run will do.",
  },
  "run.cost": {
    short: "What the AI for this run cost.",
    long: "A run that costs much more than usual is normally stuck rather than busy.",
  },
  "run.failure": {
    short: "What it was doing, why it stopped, and what you can do.",
    long:
      "Every failure answers those three questions. If it could not finish, it says so plainly " +
      "rather than reporting a job as done.",
  },
  "run.retry": {
    short: "Try this task again now.",
    long:
      "Safe even after a failure part way through: anything already booked or sent for this slot " +
      "will not happen twice.",
  },

  // ---------------------------------------------------------- credentials --
  "cred.what": {
    short: "Logins Errand may use, stored in your macOS keychain.",
    long:
      "Errand can use them; it cannot show them to you. They are never written to its database, " +
      "its logs, its screenshots, or anything it sends to an AI. Each one is tied to a single " +
      "site and refused everywhere else.",
  },
  "cred.domain": {
    short: "The only site this login may be typed into.",
    long:
      "A page that merely looks like that site is a different site, so it gets nothing. This is " +
      "the main protection against a convincing fake.",
  },
  "cred.add": {
    short: "Save a login. It goes straight to your keychain.",
  },
  "cred.delete": {
    short: "Forget this login. Tasks that use it will stop being able to sign in.",
  },

  // ------------------------------------------------------------- channels --
  "channel.telegram": {
    short: "Where run results go. This is the one Errand relies on.",
    long:
      "You can also ask it questions from your phone. It will answer them, and it deliberately " +
      "cannot start a run or approve anything, because a chat message is a poor place to " +
      "authorise something that cannot be undone.",
  },
  "channel.whatsapp": {
    short: "Best effort only. Off unless you turn it on.",
    long:
      "WhatsApp has no official way for a personal account to send messages, so this drives " +
      "WhatsApp Web through a gateway. It logs itself out and needs a QR code rescanned, which " +
      "cannot happen while you are asleep, and automated sending can get a personal number " +
      "banned. Run results always go to Telegram too, so this is never the only way you find out.",
  },
  "channel.apple": {
    short: "Needs macOS permission, which Errand asks for when you press Enable.",
    long:
      "macOS grants that permission to whichever program asks, so Errand asks while you are " +
      "looking at the screen rather than at three in the morning when nobody can click Allow.",
  },
  "automation.what": {
    short: "Something a task may do on the Mac itself, once macOS allows it.",
    long:
      "Not a way of reaching you: nothing here messages anybody. It is what a task may do on this " +
      "machine, such as writing into Notes or going through your post. macOS asks " +
      "before allowing it, and until somebody answers that question the task stops when it gets " +
      "there.",
  },
  "automation.enable": {
    short: "Ask macOS now, while you are here to answer it.",
    long:
      "macOS gives the permission to whichever program asks, so Errand asks from its background " +
      "service rather than from this window: asking from the window would grant it to the window " +
      "and the eight o'clock run would still fail. Nothing is written and nothing is read; it " +
      "asks the app a harmless question to make the prompt appear. If no prompt appears, macOS " +
      "was told no at some point, and the switch is in System Settings under Privacy and " +
      "Security, Automation.",
  },
  "channel.test": {
    short: "Send a real message to yourself, to prove the channel works.",
    long:
      "It only ever goes to you, never to anyone else, so it needs your own address on this " +
      "channel to be set first. What happened appears on this card, under the button.",
  },

  // ------------------------------------------------------------- settings --
  "settings.quiet": {
    short: "Hours when messages wait rather than arriving.",
    long:
      "Applies to messages to other people and to routine good news. A failure you asked to hear " +
      "about still comes through, because hearing at nine that the eight o'clock booking failed " +
      "is hearing too late to do anything.",
  },
  "settings.token": {
    short: "The key another program needs to talk to Errand.",
    long:
      "Errand stores only a scrambled copy, so it cannot show you this again. Give each program " +
      "its own, with only the permissions it needs.",
  },
  "settings.save_quiet": {
    short: "Save when messages should wait.",
  },
  "settings.save_telegram": {
    short: "Save the bot details. They go to your keychain and are never shown again.",
    long:
      "Until this is set, Errand has nowhere to send results and simply says nothing, which looks " +
      "like a task that never ran.",
  },
  "settings.save_whatsapp": {
    short: "Where the WhatsApp gateway is running.",
    long:
      "WhatsApp has no official way for a personal account to send messages, so this points at a " +
      "gateway you run yourself. Results always go to Telegram as well, so this is never the only " +
      "way you find out.",
  },
  "settings.save_self": {
    short: "Save where Errand should reach you on this channel. Only you, nobody else.",
    long:
      "A test message has to go somewhere, and this is the somewhere. It is not a recipient: " +
      "people a task may write to are further down the page, under 'People Errand may message'. " +
      "Nothing here lets a task message anybody new.",
  },
  "settings.add_person": {
    short: "Save someone a task may message when it finishes.",
    long:
      "Saving a person here does not let anything message them yet. You pick which tasks may, one " +
      "task at a time, which is what keeps a job from telling someone about work they never asked " +
      "to hear about.",
  },
  "settings.forget_person": {
    short: "Forget this person. Any task set to tell them stops doing so.",
  },
  "settings.daemon": {
    short: "The part that does the work, running in the background.",
    long:
      "It runs whether or not this window is open, which is what lets a task fire at eight in the " +
      "morning while you are asleep.",
  },
  // ------------------------------------------------------------- schedule --
  "sched.every": {
    short: "How often this should run.",
    long:
      "Errand writes the actual schedule for you and then shows you, underneath, what it will " +
      "really do and the next few times it will happen. If those two ever disagree, believe the " +
      "one underneath: it comes from the part that does the running.",
  },
  "sched.expr": {
    short: "A schedule expression, for something the choices above cannot say.",
    long:
      "Six fields: seconds, minutes, hours, day of month, month, day of week. An expression " +
      "copied from elsewhere usually has five and will mean something quite different here, so " +
      "check the sentence underneath before you save.",
  },
  "sched.tz": {
    short: "The clock this schedule follows.",
    long:
      "Runs happen at this local time whatever the date, so eight in the morning stays eight in " +
      "the morning across the clock change rather than drifting by an hour.",
  },
  "sched.more": {
    short: "What happens when a run is missed, and whether to start exactly on time.",
  },
  "sched.catch_up": {
    short: "What to do about a run that was missed because your Mac was off.",
    long:
      "Running late is right for most things. Skipping is right for anything time-sensitive: " +
      "nobody wants a court booked for a slot that has already passed. Making up every missed " +
      "run only suits tasks where each run does distinct work.",
  },
  "sched.jitter": {
    short: "Spread the start time slightly, instead of firing on the exact second.",
    long:
      "Useful when a booking opens at a fixed moment and every bot in the country arrives at " +
      "once. It also makes Errand look less like a machine.",
  },

  // ------------------------------------------------------------ task sites --
  "task.add_site": {
    short: "Allow this task to open one more website.",
    long:
      "Enter the plain site, such as example.com. Subdomains are included automatically, and " +
      "there are no wildcards: a * looks reasonable, saves happily, and then matches nothing.",
  },
  "task.suggested_site": {
    short: "Errand spotted this address in what you wrote. Add it with one click.",
    long:
      "Only addresses actually written in your description are offered. Errand will not guess a " +
      "company's website from its name, because a guess that lands on the wrong site is a task " +
      "pointed somewhere you never meant, possibly carrying a login.",
  },
  "task.remove_site": {
    short: "Stop this task from opening that site.",
  },
  "task.edit_schedule": {
    short: "Change when this runs on its own.",
    long:
      "Changing a schedule never replays the past. Errand will not go back and run the slots your " +
      "new schedule would have had earlier today; it starts from now.",
  },
  "task.save_schedule": {
    short: "Save it. The first run under the new schedule is the next one, not a missed one.",
  },
  "task.edit_sites": {
    short: "Change which websites this task may open.",
  },
  "task.save_sites": {
    short: "Save the list. It takes effect on the next run.",
  },
  "task.confirm_repeat": {
    short: "Change the schedule anyway, knowing it might do the same thing twice.",
    long:
      "Errand fences anything irreversible against the slot it was done for. Moving that slot " +
      "means the new one looks untouched, so work already done for the old one can happen again. " +
      "Check the site first if that would matter.",
  },
  "task.cancel_edit": {
    short: "Leave it as it was. Nothing you typed is saved.",
  },
  "task.link_person": {
    short: "Have this task message someone when it finishes.",
    long:
      "The agent can only ever message people on this list, and it cannot type an address. That " +
      "is what stops a web page from talking it into messaging someone else.",
  },
  "task.unlink_person": {
    short: "Stop telling this person about this task.",
  },
  "task.mail_grant": {
    short: "Let this one task read your mail. It can never send, reply or delete.",
    long:
      "It gets a list of who each message is from and what it is about, and it can open a " +
      "message when the summary is not enough. The model doing the job is what reads them, so " +
      "unless you have turned on 'Keep everything on this machine' on the AI page, your mail " +
      "goes to that service. Every message it opens is written into the run, by sender and " +
      "subject, so you can see afterwards exactly what it looked at. No other task is affected.",
  },
  "task.mail_file": {
    short: "Also let it move messages between mailboxes, which is how spam gets tidied away.",
    long:
      "Off unless you press it: being allowed to read your mail is not the same as being " +
      "allowed to rearrange it. Errand cannot put a message back, so a run may move each " +
      "message once and no more, and the run says which ones went where. Nothing is ever " +
      "deleted; a message that moves is still in the mailbox it was moved to.",
  },
  "task.mail_revoke": {
    short: "Take the mail away from this task. Its next run cannot even see the mail tools.",
    long:
      "This does not undo anything already read or moved, and what it read is still in the runs " +
      "for you to look at. Other tasks keep whatever they were given.",
  },
  "task.notify_when": {
    short: "Whether this person hears about runs that worked, that failed, or both. Press to change.",
    long:
      "Messages to other people wait until your quiet hours are over. Your own failure alerts do " +
      "not, because hearing at nine that the eight o'clock booking failed is hearing too late.",
  },
  "task.first_site": {
    short: "The main site decides which saved logins this task uses.",
    long:
      "Errand keeps a separate browser profile per site, so a task stays signed in between runs. " +
      "The first site in the list picks the profile; changing it means signing in again.",
  },

  // ------------------------------------------------------------------- ai --
  "ai.role": {
    short: "One of the four jobs Errand needs a model for.",
    long:
      "Only the first one, doing the task, needs a model that can call tools: Errand hands it the " +
      "browser and runs the loop itself. The other three are single questions, which any model " +
      "can answer, including one running on your own machine.",
  },
  "ai.local": {
    short: "Whether your task text leaves this machine to be answered.",
    long:
      "A model running on your own computer sees your task and nothing else does. Claude is a " +
      "service, so the wording of your task and what the agent reads on a page go to Anthropic " +
      "to be answered. Your saved logins never go to either.",
  },
  "ai.pick": {
    short: "Choose which model does this job.",
    long:
      "No preference means Errand uses whatever is switched on and working, so one model being " +
      "down does not stop a task. A model Errand has asked and found cannot use tools is shown " +
      "but cannot be picked for the task, with the reason. One nobody has checked can be picked: " +
      "not having asked is not the same as knowing it cannot.",
  },
  "ai.model": {
    short: "Which Claude does this job.",
    long:
      "Opus is the biggest: it is the best at working out a page it has never seen before, and " +
      "the most expensive to run. Sonnet is the middle one, and what Errand uses for the task " +
      "itself unless you say otherwise. Haiku is the smallest and cheapest, and is fine for " +
      "summarising a run or explaining why one failed. This changes only which model is asked; " +
      "it changes nothing about what Errand is allowed to do on your behalf.",
  },
  "ai.tools": {
    short: "Whether this model can carry out a task, as opposed to only answering questions.",
    long:
      "Carrying out a task means calling tools, because that is how Errand hands over the " +
      "browser. This says what happened when Errand asked this model to use one: it did, it " +
      "would not, or nobody has asked yet. A model that can use tools is allowed to do the job; " +
      "whether it will do it well is a separate question, and a small model will not.",
  },
  "ai.check_tools": {
    short: "Ask the models nobody has checked whether they can use a tool.",
    long:
      "One tiny question each, asking for an answer as a tool call rather than as a sentence. " +
      "Nothing else is sent, nothing is changed, and a model that is asleep or slow to load is " +
      "left as unchecked rather than written off.",
  },
  "ai.test": {
    short: "Ask it right now whether it is there, and what it can do. This can take a minute or two.",
    long:
      "The pills are the stored verdict from whenever it was last checked; this asks again this " +
      "second and says the answer in place, even when the answer has not changed. For a model " +
      "of your own it also asks the tool question, which is what decides whether it can carry " +
      "out a task.",
  },
  "ai.enable": {
    short: "Stop using this model without forgetting it.",
    long: "Its settings are kept, and any job set to prefer it falls back to something else rather than failing.",
  },
  "ai.remove": {
    short: "Forget this model. Jobs using it fall back to whatever else is available.",
  },
  "ai.scan": {
    short: "Look for a model already running, and offer whatever it finds.",
    long:
      "Nothing found is switched on for you. Finding a model and deciding to trust it are two " +
      "different things, and the second one is yours.",
  },
  "ai.scan_network": {
    short: "Also try the other machines on the network this computer is on.",
    long:
      "For a model running on a different box, such as a mini PC or a home server. Errand tries " +
      "every address on your own subnet, which takes a few seconds. Leave it off on a network " +
      "that is not yours: sweeping an office or hotel network is rude, and it looks like " +
      "something worse.",
  },
  "ai.service": {
    short: "Pick a service by name. Errand already knows its address.",
    long:
      "All of these speak the same language, so Errand reaches them the same way. Anthropic is " +
      "not in this list because Errand talks to it properly through its own connection instead.",
  },
  "ai.add_service": {
    short: "Save it, with the key going straight to your keychain.",
    long:
      "The key is checked against what that service's keys look like, then written to your " +
      "macOS keychain. It never reaches Errand's database, its logs, or this window, and Errand " +
      "cannot show it to you again.",
  },
  "ai.custom": {
    short: "For a model at an address Errand did not find or does not know.",
  },
  "ai.add_found": {
    short: "Start using this one. You choose which jobs it does afterwards.",
  },
  "ai.add": {
    short: "Add a model by address. Checked before it is saved.",
    long: "If nothing answers at that address you are told now, rather than at the moment a task needs it.",
  },
  "ai.local_only": {
    short: "Refuse to send anything to a model Errand does not reach on your own machines.",
    long:
      "This is a real restriction, not a preference: with it on, a task that nothing local can " +
      "do stops rather than quietly going to a service. A task that needs a browser is fine as " +
      "long as one of your own models can use tools. Errand will not let you turn this on until " +
      "there is a local model to use.",
  },
  "ai.key": {
    short: "Save an Anthropic key. It goes to your keychain and is never shown again.",
    long:
      "Optional, and separate from the others because Errand talks to Anthropic in its own " +
      "language rather than the common one. Errand already works without a key by using the " +
      "Claude command line tool you are signed in to; a key here bills your own account instead.",
  },

  "hold.resolve": {
    short: "Tell Errand what you found, so this task can run again.",
    long:
      "A run that started something irreversible and then died leaves nobody knowing whether it " +
      "went through. Errand stops rather than risk doing it twice. Check the site, then say what " +
      "you saw.",
  },
} as const satisfies Record<string, Hint>;

export type HintId = keyof typeof hints;

export function hint(id: HintId): Hint {
  return hints[id];
}
