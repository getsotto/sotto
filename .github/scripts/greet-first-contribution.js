"use strict";

/**
 * First-time contributor greeting for .github/workflows/greet.yml.
 *
 * An author is greeted once on their first issue and once on their first pull
 * request in this repository. An earlier item of the same kind counts whether
 * it is open, closed, or merged; items from other accounts never count; and
 * items numbered after the current one cannot block the earliest greeting.
 * Authors are identified by account id, never by login, because a login can
 * change between the event and the run (see run()).
 * A bot greeting already on the item suppresses a second post, so rerunning
 * the workflow is safe. Any API error propagates and fails the check rather
 * than being mistaken for a new contributor.
 *
 * Plain CommonJS so actions/github-script can require() it from the checked
 * out base branch - under pull_request_target that is always
 * repository-controlled code, never the contributor's.
 */

/** The issues API returns pull requests too; this field is the divider. */
function isPullRequestItem(item) {
  return Boolean(item.pull_request);
}

/**
 * True when `history` holds an item of the same kind by the same account with
 * a lower number than the item being greeted.
 */
function hasEarlierContribution(history, isPr, authorId, number) {
  return history.some(
    (item) =>
      item.user &&
      item.user.id === authorId &&
      isPullRequestItem(item) === isPr &&
      item.number < number
  );
}

/** True when a bot comment already carries the greeting's marker line. */
function hasGreeting(comments, marker) {
  return comments.some(
    (comment) =>
      comment.user &&
      comment.user.type === "Bot" &&
      typeof comment.body === "string" &&
      comment.body.includes(marker)
  );
}

/**
 * Post `issueMessage` or `prMessage` on the opened item when it is the
 * author's first of its kind and no greeting is there yet. Returns a small
 * decision record; throws on API failure so the workflow run reports it.
 */
async function run({ github, context, core, issueMessage, prMessage }) {
  const { owner, repo } = context.repo;
  const isPr = Boolean(context.payload.pull_request);
  const item = isPr ? context.payload.pull_request : context.payload.issue;
  if (!item || !item.user) {
    core.info("Event carries no issue or pull request; nothing to greet.");
    return { greeted: false, reason: "no-item" };
  }

  const kind = isPr ? "pull request" : "issue";
  const number = item.number;
  const authorId = item.user.id;
  const message = isPr ? prMessage : issueMessage;
  if (!message) {
    throw new Error(`No greeting message configured for a first ${kind}.`);
  }

  // The payload's login is the one the author had when the event fired, and
  // a run can sit queued for minutes after that. An author who renamed in
  // between (PRs #408 to #411, "TayfurYldz" to "tayfuryldz") has their
  // history listed under the new login, and GitHub answers creator=<a login
  // nobody holds> with an empty list, not an error. So look up the account's
  // current login by its id, which never changes.
  const { data: account } = await github.request("GET /user/{account_id}", {
    account_id: authorId,
  });
  const author = account.login;

  core.info(`Checking whether ${author}'s ${kind} #${number} is their first.`);

  // creator narrows the scan to this author's items; state: all counts open,
  // closed, and merged alike. paginate follows every page, and the id check
  // inside hasEarlierContribution keeps other accounts out even if the API
  // ever returns them.
  const history = await github.paginate(github.rest.issues.listForRepo, {
    owner,
    repo,
    state: "all",
    creator: author,
    per_page: 100,
  });

  if (hasEarlierContribution(history, isPr, authorId, number)) {
    core.info(`${author} already has an earlier ${kind} here; no greeting.`);
    return { greeted: false, reason: "returning-contributor" };
  }

  // A rerun or a duplicate delivery must not greet twice on the same item:
  // match the greeting's first line in any bot comment already posted.
  const marker = message
    .split("\n")
    .map((line) => line.trim())
    .find((line) => line.length > 0);
  const comments = await github.paginate(github.rest.issues.listComments, {
    owner,
    repo,
    issue_number: number,
    per_page: 100,
  });
  if (hasGreeting(comments, marker)) {
    core.info(
      `A greeting is already on ${kind} #${number}; not posting again.`
    );
    return { greeted: false, reason: "already-greeted" };
  }

  await github.rest.issues.createComment({
    owner,
    repo,
    issue_number: number,
    body: message,
  });
  core.info(`Posted the first-${kind} greeting on #${number}.`);
  return { greeted: true, reason: `first-${isPr ? "pr" : "issue"}` };
}

module.exports = { run, hasEarlierContribution, hasGreeting };
