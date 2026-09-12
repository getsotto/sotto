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

function issue(number, login, extra = {}) {
  return { number, state: "open", user: { login, type: "User" }, ...extra };
}

function pr(number, login, extra = {}) {
  return {
    number,
    state: "closed",
    user: { login, type: "User" },
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

function makeGithub({ history = [], comments = [], failures = {} } = {}) {
  const calls = { listForRepo: [], listComments: [], createComment: [] };
  const page = (items, params) =>
    items.slice(
      (params.page - 1) * params.per_page,
      params.page * params.per_page
    );
  const listForRepo = async (params) => page(history, params);
  const listComments = async (params) => page(comments, params);
  const github = {
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

test("the item itself is not its own earlier contribution", () => {
  assert.equal(hasEarlierContribution([pr(50, "alice")], true, "alice", 50), false);
});

test("hasGreeting needs a bot author carrying the marker", () => {
  assert.equal(hasGreeting([botComment("unrelated")], "marker"), false);
  assert.equal(hasGreeting([userComment("marker")], "marker"), false);
  assert.equal(hasGreeting([botComment("...marker...")], "marker"), true);
});
