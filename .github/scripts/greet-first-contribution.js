"use strict";

/**
 * First-time contributor greeting for .github/workflows/greet.yml.
 *
 * An external author is greeted once on their first pull request in this
 * repository. Repository owners, organisation members, and collaborators are
 * never greeted. An earlier pull request counts whether it is open, closed, or
 * merged; items from other accounts and issues never count; and pull requests
 * numbered after the current one cannot block the earliest greeting.
 * Authors are identified by account id, never by login, because a login can
 * change between the event and the run (see run()).
 * A bot greeting already on the pull request suppresses a second post, so
 * rerunning the workflow is safe. Any API error or incomplete history response
 * propagates and fails the check rather than being mistaken for a new
 * contributor.
 *
 * Plain CommonJS so actions/github-script can require() it from the checked
 * out base branch - under pull_request_target that is always
 * repository-controlled code, never the contributor's.
 */

const MAINTAINER_ASSOCIATIONS = new Set([
  "OWNER",
  "MEMBER",
  "COLLABORATOR",
]);

const HISTORY_ATTEMPTS = 3;

/** The issues API returns pull requests too; this field is the divider. */
function isPullRequestItem(item) {
  return Boolean(item.pull_request);
}

/**
 * True when `history` holds a pull request by the same account with a lower
 * number than the pull request being greeted.
 */
function hasEarlierContribution(history, authorId, number) {
  return history.some(
    (item) =>
      item.user &&
      item.user.id === authorId &&
      isPullRequestItem(item) &&
      item.number < number
  );
}

/**
 * True when `history` contains the opened pull request itself, proving the
 * creator-filtered issues query did not silently return an empty result.
 */
function hasCurrentPullRequest(history, authorId, number) {
  return history.some(
    (item) =>
      item.user &&
      item.user.id === authorId &&
      isPullRequestItem(item) &&
      item.number === number
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
 * Post `prMessage` on the opened pull request when it is an external author's
 * first pull request and no greeting is there yet. Returns a small decision
 * record; throws on API failure so the workflow run reports it.
 */
async function run({ github, context, core, prMessage }) {
  const { owner, repo } = context.repo;
  const item = context.payload.pull_request;
  if (!item || !item.user) {
    core.info("Event carries no pull request; nothing to greet.");
    return { greeted: false, reason: "no-item" };
  }

  const number = item.number;
  const authorId = item.user.id;
  if (!prMessage) {
    throw new Error("No greeting message configured for a first pull request.");
  }

  if (MAINTAINER_ASSOCIATIONS.has(item.author_association)) {
    core.info(
      `${item.user.login} has author_association ${item.author_association}; no greeting.`
    );
    return { greeted: false, reason: "maintainer" };
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

  core.info(
    `Checking whether ${author}'s pull request #${number} is their first.`
  );

  // creator narrows the scan to this author's items; state: all counts open,
  // closed, and merged alike. paginate follows every page, and the id check
  // inside hasEarlierContribution keeps other accounts out even if the API
  // ever returns them. GitHub's creator filter can intermittently answer 200 OK
  // with [] (PR #695), so retry until the response includes either an earlier
  // pull request or the current pull request itself.
  let history = [];
  for (let attempt = 1; attempt <= HISTORY_ATTEMPTS; attempt += 1) {
    history = await github.paginate(github.rest.issues.listForRepo, {
      owner,
      repo,
      state: "all",
      creator: author,
      per_page: 100,
    });
    if (
      hasEarlierContribution(history, authorId, number) ||
      hasCurrentPullRequest(history, authorId, number)
    ) {
      break;
    }
    if (attempt === HISTORY_ATTEMPTS) {
      throw new Error(
        `History query for ${author} did not include pull request #${number}; refusing to assume a first contribution.`
      );
    }
  }

  if (hasEarlierContribution(history, authorId, number)) {
    core.info(
      `${author} already has an earlier pull request here; no greeting.`
    );
    return { greeted: false, reason: "returning-contributor" };
  }

  // A rerun or a duplicate delivery must not greet twice on the same item:
  // match the greeting's first line in any bot comment already posted.
  const marker = prMessage
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
      `A greeting is already on pull request #${number}; not posting again.`
    );
    return { greeted: false, reason: "already-greeted" };
  }

  await github.rest.issues.createComment({
    owner,
    repo,
    issue_number: number,
    body: prMessage,
  });
  core.info(`Posted the first-pull request greeting on #${number}.`);
  return { greeted: true, reason: "first-pr" };
}

module.exports = {
  run,
  hasEarlierContribution,
  hasCurrentPullRequest,
  hasGreeting,
};
