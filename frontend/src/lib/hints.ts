/**
 * Every explanation in the app, in one place.
 *
 * The rule is that nothing goes unexplained, and a rule like that only survives
 * a growing codebase if a machine checks it. So every control is wrapped in a
 * Hint, every Hint names an entry here, and `pnpm hints:audit` fails the build
 * on a control without one or an entry nobody uses.
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
  "task.new": {
    short: "Describe a job in your own words. Errand works out how to do it.",
    long:
      "You write what you want done the way you would explain it to a person. Errand tries it " +
      "once while you watch, writes down what worked, and shows you that before it ever does the " +
      "job on its own.",
  },
  "task.status": {
    short: "Draft means unfinished. Ready means it can run. Paused means it will not.",
    long:
      "A task starts as a draft. Teaching it once produces a written plan; approving that plan " +
      "makes it ready. Paused keeps everything, including its schedule, but nothing fires.",
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
  "task.teach": {
    short: "Let it try the job once while you watch, and write down what worked.",
    long:
      "The first run works only from your description. At the end the agent writes a plan, which " +
      "you read and approve. Nothing runs on a schedule until you have approved a plan.",
  },
  "task.pause": {
    short: "Skip scheduled runs. Manual runs still work. Nothing is deleted.",
    long:
      "Pausing keeps the task, its plan, its history and its logins. When you resume, it does not " +
      "go back and do the runs it missed; it simply picks up at the next one.",
  },
  "task.activate": {
    short: "Let this run on its own schedule from now on.",
    long: "Only possible once a plan has been approved, so an unattended run always has an agreed way of doing the job.",
  },
  "task.allowed_sites": {
    short: "The only websites this task may open.",
    long:
      "Anything else is refused, including a redirect from an allowed site. A task with no sites " +
      "listed cannot browse at all. This is also what stops a page that looks like your bank " +
      "from being treated as your bank.",
  },
  "task.limits": {
    short: "Ceilings on how long a run may take and what it may spend.",
    long:
      "A run that goes round in circles is stopped rather than left going. If a run hits a " +
      "ceiling it says which one, so you can tell a genuinely slow site from a stuck agent.",
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
    short: "Agree that this is how the job should be done.",
    long:
      "This is the line between the agent having tried something once and the agent doing it " +
      "alone while you are asleep. Nothing follows a plan until you approve it.",
  },

  // ------------------------------------------------------------------ run --
  "run.timeline": {
    short: "Everything the agent did, in order, in its own words.",
    long:
      "Each line is one thing it did or decided. Where it took a screenshot you can see the page " +
      "as it saw it, with password fields covered before the picture was taken.",
  },
  "run.status": {
    short: "How this run ended.",
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
  "channel.test": {
    short: "Send a real message to yourself, to prove the channel works.",
    long: "It only ever goes to you, never to anyone else.",
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
  "settings.daemon": {
    short: "The part that does the work, running in the background.",
    long:
      "It runs whether or not this window is open, which is what lets a task fire at eight in the " +
      "morning while you are asleep.",
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
