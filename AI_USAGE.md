# AI Usage Disclosure

**Tools Used:** Claude 3.5 Sonnet / Gemini Pro (via Antigravity agentic coding environment).
**Purpose:** Used primarily for generating boilerplate Axum route handlers, initial SQL migration scripts, multi-stage Dockerfile generation, and basic data structures/models, writing overall DESIGN.md and README.md docs on top of which I made the required edits.

### Three Decisions Made Independently of AI Suggestions

1. **Concurrency Mechanism for Payments (Row-Level Locking over OCC)**
   - *What the AI proposed:* The AI initially proposed Optimistic Concurrency Control (OCC) using a `version` column on the `invoices` table to handle concurrent payments, arguing it was non-blocking.
   - *What I chose:* I explicitly overrode this and implemented Row-Level Locking (`SELECT ... FOR UPDATE`) before calling the PSP.
   - *Why:* OCC fails the core business requirement of preventing double-charges when external side-effects are involved. If two clients read version 1, both call the PSP, and both get charged, but only one database update succeeds. The customer is charged twice. Row-level locking forces the second request to wait until the first transaction completes, cleanly rejecting it before hitting the PSP.

2. **Webhook Outbox Implementation**
   - *What I chose:* A Transactional Outbox pattern strictly contained within Postgres.
   - *Why:* The assignment asks for a minimal service. Adding Redis or RabbitMQ violates the instruction against unjustified dependencies. Inserting into a `webhook_deliveries` table within the same transaction that updates the invoice state guarantees exactly-once event generation without distributed transactions.

3. **Database Migration Strategy for Enums**
   - *What the AI proposed:* Using native PostgreSQL `ENUM` types for the invoice and payment attempt statuses.
   - *What I chose:* Used a standard `VARCHAR(32)` / `TEXT` column paired with a strict `CHECK` constraint (e.g., `CHECK (status IN ('draft', 'issued', 'paid', ...))`).
   - *Why:* Postgres enums are notoriously difficult to migrate (removing or re-ordering values is a painful operational task). A `CHECK` constraint provides the exact same data-integrity guarantees at the database level while remaining trivial to alter in future migrations.

### What the AI Got Wrong (and how it was corrected)

- The AI originally botched the error responses and macro syntax in the `payments.rs` handler
- It initialized struct fields incorrectly (missing `failure_code`, `psp_ref`) for the `PaymentAttemptResponse`.
- It used compile-time checked `sqlx::query!` macros which failed `cargo check` because there was no active Postgres connection configured in the environment during the build phase. 

*Correction:* I intervened to manually rewrite the database queries using the standard runtime `sqlx::query` and `sqlx::query_as` functions, ensuring they relied on explicit `.bind()` arguments rather than the macro syntax. I also corrected the Struct instantiation.

