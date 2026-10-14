# AI Usage

## Tools and what I used them for
I used Claude Code with a Pro subscription and Opus 5.5 for the entire project. I used it to help design the database schema and implement the code in Rust. I then reviewed and verified the generated code myself to make sure everything worked as expected.


## Three decisions I made myself


1. I made the decision to add a lock state to the invoice (aka pending), inspired by how the Lightning Network uses the HTLC state as locked. 

2.  I added two partial unique indexes to the payment_attempts table to make sure each invoice can have only one pending payment attempt and one successful payment. The application already uses locking to prevent duplicate payments, but I wanted an extra layer of protection in case of bugs or race conditions. With these indexes, PostgreSQL will reject any duplicate insert that violates the rules, even if two requests somehow get past the application-level checks. This way, the database acts as the final safeguard against charging a customer twice for the same invoice. 

3. For webhooks, I decided to use an outbox table instead of sending them directly during the /pay request. When a payment succeeds, the event is saved in the same database transaction as the invoice update, and a background worker sends it later. This way, if a business endpoint is slow or down, it won't affect the payment process. The worker uses FOR UPDATE SKIP LOCKED with a 60-second lease, allowing multiple instances to process deliveries safely without claiming the same job at the same time. This also supports my goal of scaling the system horizontally. If a webhook fails, the worker retries it with increasing delays, starting at 30 seconds and going up to 12 hours, for a maximum of 9 attempts over roughly 23 hours. If all attempts fail, the event is marked as dead, and the business can still retrieve missed events through GET /events. I also reviewed the retry schedule and lease logic myself before settling on this design.


AI was very helpful in writing the return code and designing the API, and I only had to make a few changes. 

## One thing the AI got wrong
I made most of the design decisions for the invoice state flow myself. When I asked AI for suggestions to improve it, it recommended adding an uncollectible state. However, the flow it suggested had a design flaw: an invoice could be marked as uncollectible, but when a payment was attempted, it would move to the locked state and then back to open. This was incorrect because an uncollectible invoice should not become payable again through the normal payment flow. I identified this issue and verified the logic before finalizing the design. 

