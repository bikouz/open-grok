// Workflow prelude — installed ahead of every workflow script body.
// Defines the orchestration hooks (agent/parallel/pipeline/phase/log/budget)
// on top of the code-mode runtime's `tools.__wf_agent` nested call and the
// `notify` side channel. ES modules are strict; everything here is const.

const __wf = {
  phase: null,
  seq: 0,
  budgetTotal: null,
  budgetSpent: 0,
};

const __wf_errText = (e) =>
  e && typeof e.message === "string" && e.message.length > 0 ? e.message : String(e);

const __wf_notify = (payload) => {
  try {
    notify(JSON.stringify(payload));
  } catch (e) {
    // Progress display is best-effort; never fail the script over it.
  }
};

const log = (message) => {
  __wf_notify({ type: "log", message: String(message) });
};

const phase = (title) => {
  const t = String(title);
  __wf.phase = t;
  __wf_notify({ type: "phase", title: t });
};

const agent = async (prompt, opts) => {
  if (typeof prompt !== "string" || prompt.trim().length === 0) {
    throw new Error("agent(prompt, opts): prompt must be a non-empty string");
  }
  const o = opts === undefined || opts === null ? {} : opts;
  if (typeof o !== "object" || Array.isArray(o)) {
    throw new Error("agent(prompt, opts): opts must be an object");
  }
  if (__wf.budgetTotal !== null && __wf.budgetSpent >= __wf.budgetTotal) {
    throw new Error(
      "workflow token budget exhausted (" +
        __wf.budgetSpent +
        "/" +
        __wf.budgetTotal +
        " tokens): stop spawning agents and return the results gathered so far",
    );
  }
  const index = __wf.seq++;
  const call = {
    index,
    prompt,
    label: o.label === undefined || o.label === null ? null : String(o.label),
    phase:
      o.phase === undefined || o.phase === null ? __wf.phase : String(o.phase),
    schema: o.schema === undefined ? null : o.schema,
    model: o.model === undefined || o.model === null ? null : String(o.model),
    effort:
      o.effort === undefined || o.effort === null ? null : String(o.effort),
    isolation:
      o.isolation === undefined || o.isolation === null
        ? null
        : String(o.isolation),
    agent_type:
      o.agentType === undefined || o.agentType === null
        ? null
        : String(o.agentType),
  };
  const r = await tools.__wf_agent(call);
  if (r && typeof r.budget_spent === "number") {
    __wf.budgetSpent = r.budget_spent;
  }
  if (!r || r.ok !== true) {
    return null;
  }
  return r.value;
};

const parallel = (thunks) => {
  if (!Array.isArray(thunks)) {
    throw new Error("parallel(thunks): thunks must be an array of functions");
  }
  return Promise.all(
    thunks.map((thunk, i) =>
      Promise.resolve()
        .then(() => (typeof thunk === "function" ? thunk() : thunk))
        .catch((e) => {
          __wf_notify({
            type: "log",
            message: "parallel[" + i + "] failed: " + __wf_errText(e),
          });
          return null;
        }),
    ),
  );
};

const pipeline = (items, ...stages) => {
  if (!Array.isArray(items)) {
    throw new Error("pipeline(items, ...stages): items must be an array");
  }
  return Promise.all(
    items.map(async (item, index) => {
      let acc = item;
      for (const stage of stages) {
        try {
          acc = await stage(acc, item, index);
        } catch (e) {
          __wf_notify({
            type: "log",
            message: "pipeline[" + index + "] dropped: " + __wf_errText(e),
          });
          return null;
        }
      }
      return acc;
    }),
  );
};

const budget = {
  get total() {
    return __wf.budgetTotal;
  },
  spent: () => __wf.budgetSpent,
  remaining: () =>
    __wf.budgetTotal === null
      ? Infinity
      : Math.max(0, __wf.budgetTotal - __wf.budgetSpent),
};

const __wf_encodeReturn = (value) => {
  const normalized = value === undefined ? null : value;
  try {
    return "__WF_RETURN__" + JSON.stringify(normalized);
  } catch (e) {
    return "__WF_RETURN__" + JSON.stringify(String(normalized));
  }
};

// Determinism guards: workflow runs are journaled for resume, so wall-clock
// and RNG reads inside the script would break replay. Timestamps belong in
// `args`; variation belongs on the item index.
Math.random = () => {
  throw new Error(
    "Math.random() is unavailable in workflow scripts; vary agent prompts by item index instead",
  );
};
Date.now = () => {
  throw new Error(
    "Date.now() is unavailable in workflow scripts; pass timestamps in via args",
  );
};
{
  const __wf_RealDate = Date;
  globalThis.Date = new Proxy(__wf_RealDate, {
    construct(target, argsList, newTarget) {
      if (argsList.length === 0) {
        throw new Error(
          "new Date() without arguments is unavailable in workflow scripts; pass timestamps in via args",
        );
      }
      return Reflect.construct(target, argsList, newTarget);
    },
    apply() {
      throw new Error(
        "Date() is unavailable in workflow scripts; pass timestamps in via args",
      );
    },
  });
}
