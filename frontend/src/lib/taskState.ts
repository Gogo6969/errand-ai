/**
 * What a task is really doing, as opposed to the word stored on it.
 *
 * The two disagree more often than you would think. A task sits at "teaching"
 * from the moment you start teaching it until somebody approves what it
 * learned, so a run that finished hours ago still leaves the task saying
 * "Learning" -- which reads as nothing having happened at all.
 *
 * This lives in one file because it was got wrong twice: the task page was
 * taught to believe the run, and the list of tasks went on repeating the stored
 * word to anyone who never opened a task. Both screens now ask the same
 * question here, so they cannot drift apart again.
 */
import { statusLabel, type Run, type Task } from "$lib/api";
import type { HintId } from "$lib/hints";

/** Statuses that mean a run is over, one way or another. */
export const FINISHED = ["succeeded", "failed", "cancelled", "skipped"];

export const isFinished = (r?: { status: string } | null) =>
  !!r && FINISHED.includes(r.status);

export const isLive = (r?: { status: string } | null) => !!r && !isFinished(r);

export interface Headline {
  /** What to show where the status is shown. */
  text: string;
  /** "" | "ok" | "warn" | "bad", matching the pill classes in app.css. */
  cls: string;
}

/**
 * The one sentence a person should read first.
 *
 * Order matters: a run happening now beats everything, then a finished run that
 * contradicts the task, then the task's own word.
 */
export function headlineFor(task: Task | null, run?: Run | null): Headline {
  if (!task) return { text: "", cls: "" };

  if (isLive(run)) {
    return { text: statusLabel(run!.status), cls: "" };
  }

  // The case that started all this. A teaching run has ended but nobody has
  // approved the plan, so the task is still, technically, "teaching".
  if (task.status === "teaching" && isFinished(run)) {
    return run!.status === "succeeded"
      ? { text: "Waiting for your approval", cls: "warn" }
      : { text: "Teaching failed", cls: "bad" };
  }

  // A task Errand paused itself is not the same as one you paused, and the
  // difference is the whole point: one is waiting for you to check something.
  if (task.status === "paused" && task.auto_paused) {
    return { text: "Needs your attention", cls: "bad" };
  }
  const cls =
    task.status === "ready" ? "ok"
    : task.status === "paused" ? "warn"
    : "";
  return { text: statusLabel(task.status), cls };
}

/**
 * Which explanation belongs to the state this task is actually in.
 *
 * Kept beside `headlineFor` on purpose: the two answer the same question, and a
 * pill saying "Waiting for your approval" above a tooltip reciting what a draft
 * is helps nobody. They must move together.
 */
export function hintFor(task: Task | null, run?: Run | null): HintId {
  if (!task) return "task.state.draft";
  if (isLive(run)) return "task.state.running";
  if (task.status === "teaching") {
    if (isFinished(run)) {
      return run!.status === "succeeded"
        ? "task.state.awaiting_approval"
        : "task.state.teach_failed";
    }
    return "task.state.teaching";
  }
  if (task.status === "paused") {
    return task.auto_paused ? "task.state.needs_attention" : "task.state.paused";
  }
  if (task.status === "ready") return "task.state.ready";
  if (task.status === "archived") return "task.state.archived";
  return "task.state.draft";
}

/** "The teaching run could not finish", in words nobody has to decode. */
export function ranHeadline(r: Run): string {
  // Teaching and rehearsing are two different things a run can be, and it can
  // be both, so the rehearsed teach says so rather than picking one.
  const what =
    r.trigger === "teach" ? (r.rehearsal ? "The rehearsed teaching run" : "The teaching run")
    : r.rehearsal ? "The last rehearsal"
    : "The last run";
  const how =
    r.status === "succeeded" ? "worked"
    : r.status === "failed" ? "could not finish"
    : r.status === "cancelled" ? "was stopped"
    : "was skipped";
  return `${what} ${how}`;
}

/**
 * The line under a task in the list: what happened, in a few words.
 *
 * Returns null when the task's own status already says everything, so the list
 * does not repeat itself.
 */
export function subline(task: Task, run?: Run | null): string | null {
  if (!run) return null;
  if (isLive(run)) return "Running now";

  // The answer, wherever there is one, on every screen that shows a task.
  //
  // The task page was taught to lead with the answer and this was left saying
  // what the run did, so a list of tasks read as a list of activities: "wrote
  // a note", "checked the inbox". A person scanning their tasks wants to see
  // what came back, and for most of them the first line is the whole of it.
  const answered = firstLine(run.answer);
  if (answered) return answered;

  if (run.status === "failed") {
    const why = run.failure?.plain_reason ?? "";
    return why ? `${ranHeadline(run)}: ${why}` : ranHeadline(run);
  }
  if (task.status === "teaching" && run.status === "succeeded") {
    return "It wrote down how it did the job. Read that and approve it before it runs unattended.";
  }
  return run.summary || null;
}

/**
 * The first line worth reading out of an answer.
 *
 * An answer is written to be read whole on the task, so in a list it has to be
 * cut down. The first non-empty line is nearly always the sentence that says
 * what came back, and taking a fixed number of characters instead would cut
 * words in half.
 */
function firstLine(text?: string | null): string | null {
  if (!text) return null;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line) return line;
  }
  return null;
}
