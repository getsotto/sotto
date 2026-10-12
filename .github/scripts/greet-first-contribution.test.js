"use strict";

/**
 * Mocked-history tests for greet-first-contribution.js, run under
 * `node --test`. The fake paginate serves items one page at a time so
 * "history beyond one API page" is exercised, not just asserted.
 */

const assert = require("node:assert/strict");
const test = require("node:test");

const {
  run,
  hasEarlierContribution,
  hasGreeting,
} = require("./greet-first-contribution.js");

const ISSUE_MESSAGE =
  "Thanks for opening your first issue in Sotto, and welcome.\n\nIssue body.";
const PR_MESSAGE =
  "Thanks for your first pull request to Sotto, and welcome.\n\nPR body.";

// Every login gets a stable numeric id, as on GitHub. A test that renames an
// account passes the same id under a different login.
const ACCOUNT_IDS = new Map();

function accountId(login) {
  if (!ACCOUNT_IDS.has(login)) {
    ACCOUNT_IDS.set(login, 1000 + ACCOUNT_IDS.size);
  }
  return ACCOUNT_IDS.get(login);
}

function user(login, id = accountId(login)) {
  return { login, id, type: "User" };
}

function issue(number, login, extra = {}) {
  return { number, state: "open", user: user(login), ...extra };
}

function pr(number, login, extra = {}) {
  return {
    number,
    state: "closed",
    user: user(login),
    pull_request: { merged_at: null },
    ...extra,
  };
}

function botComment(body) {
  return { user: { login: "github-actions[bot]", type: "Bot" }, body };
}

function userComment(body) {
  return { user: { login: "someone", type: "User" }, body };
}

/**
 * `logins` maps an account id to its current login, standing in for
 * GET /user/{account_id}; unlisted ids resolve through ACCOUNT_IDS. With
 * `filterByCreator`, listForRepo filters the way GitHub does: logins match
 * case-insensitively, and a login no account holds any more returns an empty
 * list rather than an error.
 */
function makeGithub({
  history = [],
  comments = [],
  failures = {},
  logins = {},
  filterByCreator = false,
} = {}) {
  const calls = {
    getUser: [],
    listForRepo: [],
    listComments: [],
    createComment: [],
  };
  const page = (items, params) =>
    items.slice(
      (params.page - 1) * params.per_page,
      params.page * params.per_page
    );
  const listForRepo = async (params) => {
    const creator = params.creator.toLowerCase();
    const items = filterByCreator
      ? history.filter((item) => item.user.login.toLowerCase() === creator)
      : history;
    return page(items, params);
  };
  const listComments = async (params) => page(comments, params);
  const github = {
    request: async (route, params) => {
      assert.equal(route, "GET /user/{account_id}");
      calls.getUser.push(params);
      if (failures.getUser) {
        throw new Error(failures.getUser);
      }
      const login =
        logins[params.account_id] ??
        [...ACCOUNT_IDS].find(([, id]) => id === params.account_id)?.[0];
      return { data: { login, id: params.account_id } };
    },
    rest: {
      issues: {
        listForRepo,
        listComments,
        createComment: async (params) => {
          calls.createComment.push(params);
        },
      },
    },
    // Stands in for octokit's paginate: request pages until a short one, so
    // the tests only pass if every page is followed.
    paginate: async (endpoint, params) => {
      const name =
        endpoint === listForRepo ? "listForRepo" : "listComments";
      calls[name].push(params);
      if (failures[name]) {
        throw new Error(failures[name]);
      }
      const items = [];
      for (let pageNumber = 1; ; pageNumber += 1) {
        const chunk = await endpoint({ ...params, page: pageNumber });
        items.push(...chunk);
        if (chunk.length < params.per_page) {
          return items;
        }
      }
    },
  };
  return { github, calls };
}

function makeContext(item, isPr) {
  return {
    repo: { owner: "getsotto", repo: "sotto" },
    payload: isPr ? { pull_request: item } : { issue: item },
  };
}

const core = { info() {}, setFailed() {} };

function args(overrides) {
  return {
    github: overrides.github,
    context: overrides.context,
    core,
    issueMessage: ISSUE_MESSAGE,
    prMessage: PR_MESSAGE,
  };
}

test("a new author's first pull request is greeted", async () => {
  const { github, calls } = makeGithub();
  const result = await run(
    args({ github, context: makeContext(pr(50, "newbie"), true) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment.length, 1);
  assert.equal(calls.createComment[0].issue_number, 50);
  assert.equal(calls.createComment[0].body, PR_MESSAGE);
});

test("a returning pull request author is not greeted, even with no issues", async () => {
  // The regression this replaces: upstream OR'd the first-issue check in, so
  // an author with earlier pull requests but no issues was greeted every time.
  const { github, calls } = makeGithub({
    history: [
      pr(10, "alice"),
      pr(42, "alice", { pull_request: { merged_at: "2026-09-01T00:00:00Z" } }),
    ],
  });
  const result = await run(
    args({ github, context: makeContext(pr(214, "alice"), true) })
  );
  assert.equal(result.greeted, false);
  assert.equal(result.reason, "returning-contributor");
  assert.equal(calls.createComment.length, 0);
});

test("an author with only issues is greeted on their first pull request", async () => {
  const { github, calls } = makeGithub({
    history: [issue(3, "alice"), issue(9, "alice", { state: "closed" })],
  });
  const result = await run(
    args({ github, context: makeContext(pr(50, "alice"), true) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment[0].body, PR_MESSAGE);
});

test("an author with only pull requests is greeted on their first issue", async () => {
  const { github, calls } = makeGithub({ history: [pr(5, "alice")] });
  const result = await run(
    args({ github, context: makeContext(issue(50, "alice"), false) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment[0].body, ISSUE_MESSAGE);
});

test("items numbered after the current one do not block the greeting", async () => {
  const { github, calls } = makeGithub({ history: [pr(99, "alice")] });
  const result = await run(
    args({ github, context: makeContext(pr(50, "alice"), true) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment.length, 1);
});

test("another author's history does not affect eligibility", async () => {
  const { github, calls } = makeGithub({
    history: [pr(1, "bob"), issue(2, "carol")],
  });
  const result = await run(
    args({ github, context: makeContext(pr(50, "alice"), true) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment.length, 1);
});

test("history beyond the first page is still searched", async () => {
  // 104 issues push an earlier pull request onto the second page; reading
  // only page one would wrongly greet this returning contributor.
  const history = Array.from({ length: 104 }, (_, i) =>
    issue(i + 1, "alice")
  );
  history.push(pr(150, "alice"));
  const { github, calls } = makeGithub({ history });
  const result = await run(
    args({ github, context: makeContext(pr(200, "alice"), true) })
  );
  assert.equal(result.greeted, false);
  assert.equal(calls.createComment.length, 0);
});

test("history is queried for the item's author across all states", async () => {
  const { github, calls } = makeGithub();
  await run(args({ github, context: makeContext(pr(50, "alice"), true) }));
  assert.deepEqual(calls.listForRepo[0], {
    owner: "getsotto",
    repo: "sotto",
    state: "all",
    creator: "alice",
    per_page: 100,
  });
});

test("an existing bot greeting on the item is not repeated", async () => {
  const { github, calls } = makeGithub({
    comments: [botComment(PR_MESSAGE)],
  });
  const result = await run(
    args({ github, context: makeContext(pr(50, "newbie"), true) })
  );
  assert.equal(result.greeted, false);
  assert.equal(result.reason, "already-greeted");
  assert.equal(calls.createComment.length, 0);
});

test("a human comment quoting the greeting does not suppress it", async () => {
  const { github, calls } = makeGithub({
    comments: [userComment(`quoting: ${PR_MESSAGE}`)],
  });
  const result = await run(
    args({ github, context: makeContext(pr(50, "newbie"), true) })
  );
  assert.equal(result.greeted, true);
  assert.equal(calls.createComment.length, 1);
});

test("a history API failure propagates instead of assuming a newcomer", async () => {
  const { github, calls } = makeGithub({
    failures: { listForRepo: "boom" },
  });
  await assert.rejects(
    run(args({ github, context: makeContext(pr(50, "alice"), true) })),
    /boom/
  );
  assert.equal(calls.createComment.length, 0);
});

test("a comments API failure propagates instead of risking a duplicate", async () => {
  const { github, calls } = makeGithub({
    failures: { listComments: "boom" },
  });
  await assert.rejects(
    run(args({ github, context: makeContext(pr(50, "alice"), true) })),
    /boom/
  );
  assert.equal(calls.createComment.length, 0);
});

// PRs #408 to #411: the author changed "TayfurYldz" to "tayfuryldz" while
// those runs were queued, so each event still carried the old casing while
// the API returned their history under the new one. Comparing logins greeted
// a contributor with at least nine earlier pull requests, four times over.
test("a login whose case changed after the event is still recognised", async () => {
  const id = 238304586;
  const { github, calls } = makeGithub({
    history: [
      pr(258, "tayfuryldz", { user: user("tayfuryldz", id) }),
      pr(407, "tayfuryldz", { user: user("tayfuryldz", id) }),
    ],
    logins: { [id]: "tayfuryldz" },
    filterByCreator: true,
  });
  const event = pr(409, "TayfurYldz", { user: user("TayfurYldz", id) });
  const result = await run(args({ github, context: makeContext(event, true) }));
  assert.equal(result.greeted, false);
  assert.equal(result.reason, "returning-contributor");
  assert.equal(calls.createComment.length, 0);
});

// The same queue delay across a full rename: the event's login now belongs to
// nobody, and GitHub answers creator=<unknown login> with an empty list, not
// an error, so querying by the event's login would find no history at all.
test("a full rename after the event still finds the author's history", async () => {
  const id = 5150;
  const { github, calls } = makeGithub({
    history: [pr(12, "new-name", { user: user("new-name", id) })],
    logins: { [id]: "new-name" },
    filterByCreator: true,
  });
  const event = pr(50, "old-name", { user: user("old-name", id) });
  const result = await run(args({ github, context: makeContext(event, true) }));
  assert.deepEqual(calls.getUser, [{ account_id: id }]);
  assert.equal(calls.listForRepo[0].creator, "new-name");
  assert.equal(result.greeted, false);
  assert.equal(calls.createComment.length, 0);
});

test("an earlier item counts by account id, whatever its login", () => {
  const history = [pr(10, "Alice", { user: user("Alice", 1) })];
  assert.equal(hasEarlierContribution(history, true, 1, 50), true);
  // A released login can be claimed by another account; its items stay theirs.
  assert.equal(hasEarlierContribution(history, true, 2, 50), false);
});

test("an account lookup failure propagates instead of assuming a newcomer", async () => {
  const { github, calls } = makeGithub({
    failures: { getUser: "boom" },
  });
  await assert.rejects(
    run(args({ github, context: makeContext(pr(50, "alice"), true) })),
    /boom/
  );
  assert.equal(calls.listForRepo.length, 0);
  assert.equal(calls.createComment.length, 0);
});

test("the item itself is not its own earlier contribution", () => {
  const item = pr(50, "alice");
  assert.equal(hasEarlierContribution([item], true, item.user.id, 50), false);
});

test("hasGreeting needs a bot author carrying the marker", () => {
  assert.equal(hasGreeting([botComment("unrelated")], "marker"), false);
  assert.equal(hasGreeting([userComment("marker")], "marker"), false);
  assert.equal(hasGreeting([botComment("...marker...")], "marker"), true);
});
