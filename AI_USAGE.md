# AI Usage

## Tools and what they were used for

I used OpenAI Codex for this project. It helped with:

- writing most of the Rust code, the database migration, and the Docker setup
- writing the integration tests
- drafting the initial README, DESIGN.md, and API.md
- explaining how the API, database, payment worker, and webhook worker connect,
  and where the service can fail: duplicate payment requests, PSP timeout,
  network error, and idempotency key reuse

I reviewed all of it before submitting, and the three decisions below are
mine, made by reviewing what Codex proposed and either agreeing with it for a
specific reason or overriding it.

## Three decisions I made myself

1. **Decision:** Keep the invoice state machine at three states: `open`,
   `processing`, and `paid`, with no `void` state.
   **AI suggestion:** Codex proposed this three-state model as part of the
   initial implementation, without a cancellation state.
   **My choice and reason:** I reviewed this against the requirements and kept
   it. Invoices in this system are issued immediately with no draft or approval
   step, and there is no cancellation or collections policy in scope. Adding
   `void` without the workflow behind it would just be an unused enum value,
   and the assignment explicitly rewards restraint over extra states I cannot
   justify.

2. **Decision:** Idempotency keys have no expiry, and I decided not to add one
   for this submission.
   **AI suggestion:** Codex implemented keys as permanently valid per business,
   with no TTL or cleanup.
   **My choice and reason:** I considered adding a TTL, such as expiring keys
   after 24 to 48 hours, since indefinite growth of `payment_attempts` is a real
   operational cost. I decided against implementing it here because it adds a
   cleanup job and is not exercised by any required test. It is the first thing
   I would add before this went to production, and I noted it as a gap rather
   than silently leaving it out.

3. **Decision:** On `tok_network_error`, never auto-reopen the invoice; keep it
   `processing` until a clear PSP result arrives.
   **AI suggestion:** Codex implemented this as leaving the attempt pending and
   allowing the retry path to keep trying until a clear result exists.
   **My choice and reason:** I agreed with this after thinking through the
   alternative: if I auto-reopened the invoice after some timeout, a retry could
   double-charge the customer if the original request eventually succeeds. An
   unresolved result is not the same as a failure, so I kept it pending rather
   than guessing. The tradeoff is that a true outage needs a human to notice and
   investigate. There is no operator override built in, which I flagged in
   DESIGN.md's production-readiness section.

## One thing the AI got wrong or that I verified

I did not find a case where Codex's output was factually wrong in a way I had
to fix. What I did instead was verify the correctness claims directly rather
than take them on faith:

- I ran the full test suite, including the concurrency, idempotency, and
  PSP-failure tests.
- I manually ran the `docker compose up` flow from a clean checkout and walked
  through the curl examples in README.md by hand.
- I specifically checked that the retry-interval SQL binds a Rust integer to a
  Postgres double-precision parameter with an explicit cast, since that is a
  common silent-mismatch bug in this kind of code.

## Remaining work

The video walkthrough is linked in README.md under Demo Video. The main
production gaps I chose not to implement are documented in DESIGN.md.
