// Move the ACPilot Roadmap project's pull-request cards.
//
// The roadmap is a Projects v2 board, and a board is only worth reading if it is
// true. The issue lane is GitHub's: its built-in project workflows already set a
// card to Todo when the issue is added or reopened and to Done when it closes. No
// built-in can express the middle of a pull request's life, so that much is
// stated here and called from `.github/workflows/roadmap.yml` on PR events:
//
//   a draft PR                    -> In progress
//   a PR ready for review         -> In review
//   a PR merged                   -> Done
//   a PR closed without merging   -> left where it is; it is not finished work
//
// The `issue` and `backfill` commands below are for filling in what an event
// missed, and for the one-time import of the issues made from TODO.md:
//
//   GH_TOKEN=$(gh auth token) node .github/scripts/roadmap.mjs backfill
//
// A token is needed with the `project` scope: `GITHUB_TOKEN` cannot see a project
// owned by a user account. Without one the workflow skips the job rather than
// passing while doing nothing, and this script exits 0 if a token it was given
// turns out to lack the scope, because a roadmap that cannot be updated is not a
// reason to fail a pull request's checks.

const API = 'https://api.github.com';
const GRAPHQL = `${API}/graphql`;

const PROJECT_TITLE = process.env.ROADMAP_PROJECT ?? 'ACPilot Roadmap';
const REPO = process.env.GITHUB_REPOSITORY ?? '';
const TOKEN = process.env.GH_TOKEN ?? process.env.GITHUB_TOKEN ?? '';

// Column names are matched case-insensitively, and the first spelling that the
// project actually has wins. A board with different headings still gets updated
// instead of silently drifting.
//
// `review` falls back to `in progress`: the board shipped with Todo / In Progress /
// Done, and a pull request waiting on a review is in progress by any reading. Add
// an `In Review` column and the script prefers it without a code change.
const COLUMNS = {
  todo: ['to do', 'todo', 'backlog'],
  active: ['in progress', 'in development', 'doing'],
  review: ['in review', 'review', 'in progress'],
  done: ['done', 'complete', 'completed'],
};

function fail(message) {
  console.error(`roadmap: ${message}`);
  process.exit(1);
}

if (!TOKEN) fail('no token: set GH_TOKEN (needs the `project` scope)');
if (!REPO.includes('/')) fail('set GITHUB_REPOSITORY=owner/repo');

const [owner, repo] = REPO.split('/');

async function graphql(query, variables) {
  const response = await fetch(GRAPHQL, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
      'User-Agent': 'acpilot-roadmap',
    },
    body: JSON.stringify({ query, variables }),
  });
  const body = await response.json();
  if (body.errors?.length) {
    throw new Error(body.errors.map((e) => e.message).join('; '));
  }
  if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
  return body.data;
}

/// `#123` and `123` both name an item; the caller should not have to care.
function number(value) {
  const parsed = Number(String(value).replace(/^#/, ''));
  if (!Number.isInteger(parsed) || parsed <= 0) fail(`not an item number: ${value}`);
  return parsed;
}

async function findProject() {
  const data = await graphql(
    `query ($owner: String!, $repo: String!) {
      repository(owner: $owner, name: $repo) {
        owner {
          login
          ... on User { projectsV2(first: 100) { nodes { id number title } } }
          ... on Organization { projectsV2(first: 100) { nodes { id number title } } }
        }
      }
    }`,
    { owner, repo },
  );
  const projects = data.repository.owner.projectsV2?.nodes ?? [];
  const project = projects.find((p) => p.title.toLowerCase() === PROJECT_TITLE.toLowerCase());
  if (!project) {
    const seen = projects.map((p) => p.title).join(', ') || 'none';
    throw new Error(
      `no project titled "${PROJECT_TITLE}" for ${data.repository.owner.login} (saw: ${seen})`,
    );
  }
  return project;
}

async function findStatusField(projectId) {
  const data = await graphql(
    `query ($id: ID!) {
      node(id: $id) {
        ... on ProjectV2 {
          fields(first: 50) {
            nodes {
              ... on ProjectV2SingleSelectField { id name options { id name } }
            }
          }
        }
      }
    }`,
    { id: projectId },
  );
  // Fields the fragment does not cover — text, number, iteration — come back as
  // empty objects, so the name is the only reliable way to pick one out.
  const fields = data.node.fields.nodes.filter((field) => field?.name);
  const status = fields.find((field) => field.name.toLowerCase() === 'status');
  if (!status) throw new Error('the project has no single-select "Status" field');

  const options = new Map(status.options.map((o) => [o.name.toLowerCase(), o.id]));
  const pick = (kind) => COLUMNS[kind].map((n) => options.get(n)).find(Boolean);
  return {
    id: status.id,
    pick,
    missing: (kind) => `${kind}: ${COLUMNS[kind].find((n) => options.has(n)) ?? 'none'}`,
  };
}

/// The project item for a piece of content, if the board already holds it.
/// `addProjectV2ItemById` does not quietly deduplicate, so the check is what
/// keeps a re-run or a re-labelled event from stacking duplicates.
async function existingItem(contentId, projectId) {
  const data = await graphql(
    `query ($id: ID!) {
      node(id: $id) {
        ... on Issue { projectItems(first: 50) { nodes { id project { id } } } }
        ... on PullRequest { projectItems(first: 50) { nodes { id project { id } } } }
      }
    }`,
    { id: contentId },
  );
  const items = data.node?.projectItems?.nodes ?? [];
  return items.find((item) => item.project.id === projectId)?.id ?? null;
}

async function addItem(contentId, projectId) {
  const data = await graphql(
    `mutation ($project: ID!, $content: ID!) {
      addProjectV2ItemById(input: { projectId: $project, contentId: $content }) {
        item { id }
      }
    }`,
    { project: projectId, content: contentId },
  );
  return data.addProjectV2ItemById.item.id;
}

async function setColumn(projectId, itemId, fieldId, optionId) {
  await graphql(
    `mutation ($project: ID!, $item: ID!, $field: ID!, $option: String!) {
      updateProjectV2ItemFieldValue(input: {
        projectId: $project
        itemId: $item
        fieldId: $field
        value: { singleSelectOptionId: $option }
      }) { projectV2Item { id } }
    }`,
    { project: projectId, item: itemId, field: fieldId, option: optionId },
  );
}

/// The state the board should show, from the item as GitHub sees it, so an event
/// payload that has since been overtaken cannot write a stale column.
function columnFor(item) {
  if (item.__typename === 'Issue') {
    return item.state === 'OPEN' ? 'todo' : 'done';
  }
  if (item.merged || item.state === 'MERGED') return 'done';
  if (item.state === 'CLOSED') return null;
  return item.isDraft ? 'active' : 'review';
}

async function loadItem(kind, itemNumber) {
  const data = await graphql(
    `query ($owner: String!, $repo: String!, $number: Int!) {
      repository(owner: $owner, name: $repo) {
        issueOrPullRequest(number: $number) {
          __typename
          ... on Issue { id state }
          ... on PullRequest { id state isDraft merged }
        }
      }
    }`,
    { owner, repo, number: itemNumber },
  );
  const item = data.repository.issueOrPullRequest;
  if (!item) fail(`#${itemNumber} does not exist in ${REPO}`);
  const wanted = kind === 'pr' ? 'PullRequest' : 'Issue';
  if (item.__typename !== wanted) fail(`#${itemNumber} is a ${item.__typename}, not a ${wanted}`);
  return { ...item, number: itemNumber };
}

/// Place one item on the board and give it the column its state earns.
async function sync(item, project, status) {
  const label = `#${item.number}`;
  let itemId = await existingItem(item.id, project.id);
  if (!itemId) {
    itemId = await addItem(item.id, project.id);
    console.log(`${label}: added to ${project.title}`);
  }

  const column = columnFor(item);
  if (!column) {
    console.log(`${label}: closed without merging, column left alone`);
    return;
  }
  const option = status.pick(column);
  if (!option) {
    console.log(`${label}: project has no column for "${column}", status left alone`);
    return;
  }
  await setColumn(project.id, itemId, status.id, option);
  console.log(`${label}: ${column}`);
}

async function rest(path) {
  const response = await fetch(`${API}${path}`, {
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      Accept: 'application/vnd.github+json',
      'User-Agent': 'acpilot-roadmap',
    },
  });
  if (!response.ok) throw new Error(`${path}: ${response.status} ${response.statusText}`);
  return response.json();
}

/// Every issue the repository has, oldest first, for the one-time backfill after
/// the TODO.md import. Pull requests are left out: the import only made issues,
/// and a merged dependency bump is not on the roadmap. From here on the workflow
/// adds each pull request as it happens.
async function backfill(project, status) {
  let page = 1;
  let seen = 0;
  for (;;) {
    const batch = await rest(`/repos/${REPO}/issues?state=all&per_page=100&page=${page}`);
    if (batch.length === 0) break;
    for (const entry of batch) {
      if (entry.pull_request) continue;
      const item = await loadItem('issue', entry.number);
      await sync(item, project, status);
      seen += 1;
    }
    if (batch.length < 100) break;
    page += 1;
  }
  console.log(`roadmap: ${seen} items considered`);
}

async function main() {
  // A closed-unmerged PR is not finished work, but leaving it off the board
  // would hide it from whoever wonders where it went.
  const project = await findProject();
  const status = await findStatusField(project.id);

  const [command, target] = process.argv.slice(2);
  if (command === 'backfill') return backfill(project, status);
  if (command === 'pr' || command === 'issue') {
    return sync(await loadItem(command, number(target)), project, status);
  }
  fail('usage: roadmap.mjs <pr|issue> <number> | backfill');
}

main().catch((error) => {
  // Distinguish "this token cannot see projects" from a real mistake: one is a
  // missing secret, the other is a bug, and only the second should be loud.
  const message = String(error.message ?? error);
  if (/scope|not accessible|Could not resolve to a ProjectV2|Resource not accessible/i.test(message)) {
    console.log(`roadmap: skipped, token cannot update the project (${message})`);
    return;
  }
  fail(message);
});
