/**
 * Keeping a screen alive while the background service is not there.
 *
 * Every page loads what it needs once, when it mounts. If that one fetch fails
 * the page stays as it was for ever: on the task list that meant a red banner
 * over the words "Nothing here yet" while twenty-two tasks sat on disk, and the
 * only way out was quitting the app and opening it again.
 *
 * The window opens onto a service that is not listening yet more often than it
 * should: after an update, after a restart, after the Mac wakes. So a failed
 * load is not an ending, it is a wait.
 *
 * One implementation, in a runes module, because six copies of a backoff is six
 * things to get wrong and the sixth would be the one nobody tested.
 */
import { ApiError } from "$lib/api";

/** How long to wait before each retry. Runs out on purpose: a page that
 *  retries for ever is a page quietly making requests all night. */
const WAITS = [400, 800, 1500, 2500, 4000, 8000];

export interface Reconnect {
  /** Is another attempt already on its way? */
  readonly retrying: boolean;
  /** Load now. Pass true when a person asked for it, which starts over. */
  run: (manual?: boolean) => Promise<void>;
  /** Drop any pending attempt. Call this when the page goes away. */
  stop: () => void;
}

/**
 * Wrap a page's load so it comes back by itself.
 *
 * `load` must throw when it fails: swallowing the error is what made every one
 * of these pages unable to tell a bad fetch from an empty answer. `report` is
 * handed the message to show, or null once it works, so the page keeps owning
 * what it says and can put its own failures in the same place.
 */
export function reconnecting(
  load: () => Promise<unknown>,
  report: (problem: string | null) => void,
): Reconnect {
  let attempt = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let retrying = $state(false);

  async function run(manual = false) {
    if (manual) {
      attempt = 0;
      clearTimeout(timer);
    }
    try {
      await load();
      attempt = 0;
      retrying = false;
      report(null);
    } catch (e) {
      report(e instanceof ApiError ? e.message : String(e));
      const wait = WAITS[attempt];
      if (wait === undefined) {
        retrying = false;
        return;
      }
      attempt += 1;
      retrying = true;
      clearTimeout(timer);
      timer = setTimeout(() => void run(), wait);
    }
  }

  return {
    get retrying() {
      return retrying;
    },
    run,
    stop: () => clearTimeout(timer),
  };
}
