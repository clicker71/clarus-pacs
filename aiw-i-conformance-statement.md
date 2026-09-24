# Clarus IHE AIW-I Conformance Statement

**Status: PUBLISHED conformance claim (2026-09-24).** Sections marked
CONFORMANT reflect shipped, tested behavior. The rows marked PLANNED
are declared work and are few and named: the Push workflow model (D8)
and the Type 3 model-version provenance additions. Every obligation row
of the two claimed actors is CONFORMANT today.

Profile: IHE Radiology Technical Framework Supplement
**AI Workflow for Imaging (AIW-I)**, Trial Implementation, Rev 1.1,
2020-08-06. AIW-I has no options for the claimed actors except the
Instance Availability Option, which Clarus does not claim.

Trial Implementation note: AIW-I is a Trial Implementation profile and
may be revised by IHE. This statement claims conformance to Rev 1.1 as
published 2020-08-06; if the profile is revised, this statement will be
updated accordingly.

| Item | Value |
|---|---|
| Product | Clarus (origin server) + clinfer (processing sidecar) |
| Claimed AIW-I actors | Task Manager (Clarus), Task Performer (clinfer) |
| Grouped actors | Image Manager (grouped with the Task Manager, **DICOMweb retrieve/store surface only**; the DIMSE responder surface lives in the clbridge product with its own conformance statement and is not part of this claim). ITI Consistent Time / Time Client (with Task Performer, via system clock per site NTP policy) |
| Workflow models claimed | Task Performer: **Pull** and **Triggered Pull**. Task Manager: Pull and Triggered Pull; Push PLANNED |
| Not claimed | Watcher, Procedure Reporter, Report Manager, Instance Availability Option, Push performer mode |

---

## Terminology (for readers new to AIW-I)

- **Workitem** - a DICOM Unified Procedure Step (UPS) object: one task and
  everything around it (who requested it, what its inputs and outputs
  are, what state it is in). One workitem = one AI inference task.
- **Task Manager** - the system that creates, stores and manages
  workitems (in Clarus: the origin server). The origin server creates
  workitems under the capability PS3.18 Section 11.1 grants it ("User
  agents and origin servers can create Workitems"); the STOW trigger
  for that creation is a product feature, not a conformance
  requirement.
- **Task Performer** - the system that claims, executes and completes
  workitems (in Clarus: clinfer).
- **Pull workflow** - the performer periodically asks the manager for
  work it can do, then claims it.
- **Triggered Pull workflow** - the manager notifies the performer when
  work becomes available, and the performer then claims it over the same
  UPS-RS transactions Pull uses. The notification is a hint, not a
  delivery: the workitem itself is never copied to the performer, so a
  lost notification costs latency and not work.
- **Push workflow** - the manager sends a copy of the workitem to the
  performer. Clarus does not claim this model; the supported notification
  mechanism is the Triggered Pull one above.

---

## 1. Task Manager (Clarus origin server)

AIW-I Table 50.1-1 makes RAD-80..RAD-87 and RAD-109 required for the
Task Manager. AIW-I 50.1.1.2 additionally requires support of all three
workflow models.

Clarus acts as the Task Manager through the origin server. Its
workitem creation is exercised under the capability PS3.18 Section 11.1
grants origin servers: "User agents and origin servers can create
Workitems". The STOW-triggered creation is a product feature built on
that right, not a conformance requirement: after a durable STOW the
origin server creates one UPS-RS workitem for the study (a
configuration-controlled feature, off by default), applies no modality
filter, and the claiming sidecar cancels a non-matching study itself.

| Transaction | RESTful semantics (PS3.18) | Status |
|---|---|---|
| RAD-80 Create UPS Workitem | Create (11.4) | **CONFORMANT** (responder and initiator). Input Readiness State stored and required at creation; the internal requestor (STOW-triggered CAD workitem) populates READY, MEDIUM priority, Input Information Sequence. |
| RAD-81 Query UPS Workitems | Search (11.9) | **CONFORMANT**. Matching keys: workitem UID, Procedure Step Label (0074,1204), Scheduled Station AE Title (0040,0001), Patient ID, Procedure Step State. The AIW-I station key is (0040,4025) Code Value, accepted as an alias of (0040,0001) - the two name one fact (AIW-I 4.80.4.1.2.1): a search carrying either form reaches the same workitem, and (0040,0001) decides when a single search carries both. Patient/procedure/task-oriented keys (0040,A370, 0040,4018, 0032,1064) are not matching keys. |
| RAD-82 Claim UPS Workitem | Change Workitem State (11.7) to IN PROGRESS with Transaction UID | **CONFORMANT** (PS3.4 Table CC.1.1-2 state machine, locking UID enforced). |
| RAD-83 Get UPS Workitem | Retrieve Workitem (11.5) | **CONFORMANT** (initiator and responder). |
| RAD-84 Update UPS Workitem | Update (11.6) | **CONFORMANT** (locking UID enforced). |
| RAD-85 Complete UPS Workitem | Change Workitem State (11.7) to COMPLETED/CANCELED | **CONFORMANT**. Final-state retention: final workitems are swept only after the configured retention window (>= 24 h by default), satisfying 4.85.4.1.3. |
| RAD-86 Manage UPS Subscription | Create/Suspend/Delete Subscription (11.10/11.11) | **CONFORMANT**. Global (1.2.840.10008.5.1.4.34.5) and Filtered (34.5.1) subscriptions; subscribe, unsubscribe, suspend-global; the `?deletionlock` query parameter creates a subscription carrying a Deletion Lock (PS3.18 11.10), honoured by the retention sweep for a configurable lifetime whose default is the profile's 24-hour floor (AIW-I 4.85.4.1.3). The subscription registry is process-lifetime, so a restart forgets subscriptions and their locks - a declared behavior, not a deviation: the profile sets no persistence requirement, and the durable path is UPS Search. |
| RAD-87 Send UPS Notification | Event reports over the open WebSocket channel | **CONFORMANT**. Event Reports are delivered as text frames on the notification connection (section 3), serialised by the same writer the SSE extension uses. |
| RAD-109 Open Event Channel | WebSocket notification connection | **CONFORMANT**. `GET /dicomweb/subscribers/{requester}` with the RFC 6455 upgrade headers; hand-rolled responder (no new dependencies). |

Actor-level: Pull and Triggered Pull workflow CONFORMANT; Push PLANNED.

## 2. Task Performer (clinfer)

AIW-I 50.1.1.3: the performer implements one or more of the three
workflow models. Clarus claims **Pull** and **Triggered Pull**.

| Obligation | Status |
|---|---|
| Query UPS Workitems (initiator) | **CONFORMANT**. The station-oriented search of 4.81.4.1.2.2 is issued with the performer's own (0040,0001); the local rule filtering remains as a fallback for work that is not tied to a station, and an origin that refuses the key degrades to the unfiltered search rather than to no work. |
| Get UPS Workitem (initiator) | **CONFORMANT**. |
| Claim UPS Workitem (responder role via Change State) | **CONFORMANT**: claim with generated Transaction UID works; the claim guards of 4.82.4.1.1 refuse a workitem whose assignment attributes (0040,4025, 0040,4036, 0040,4026) name another performer, and refuse Input Readiness State INCOMPLETE. An absent or empty assignment names nobody, which is an offer to whoever can serve the work; a refusal is a decline that leaves the workitem SCHEDULED, not an error. |
| Update UPS Workitem | **CONFORMANT** (locking UID). |
| Open Event Channel (initiator, Triggered Pull) | **CONFORMANT**. The performer opens `GET /dicomweb/subscribers/{requester}` with the RFC 6455 upgrade headers AFTER registering its subscription, and reads Event Reports from it. A dropped channel is reconnected with exponential backoff, and the reconnect re-runs the startup search so the window in which notifications were missed is recovered by UPS Search rather than lost. |
| Receive UPS Notification (initiator, Triggered Pull) | **CONFORMANT**. A report whose state is SCHEDULED wakes the claim path; any other state, an unreadable body, or a report with no workitem UID is ignored as ordinary traffic. The claim itself is the ordinary Change State transaction, so a duplicate or lost notification costs latency and never correctness. |
| Complete UPS Workitem with all created instances listed in Output Information Sequence (0040,4033) before the final state | **CONFORMANT**. |
| Performed Workitem Code Sequence (0040,4019) populated on update | **CONFORMANT**. The completed transition echoes (0040,4018) as 4.84.4.1.2.1 requires ("the same code as Scheduled, if present"); a workitem that scheduled no code has nothing to echo and the sequence is omitted. |
| Performed Station Name Code Sequence (0040,4028) populated on update | **CONFORMANT**. Code Value is the configured station AE Title, Code Meaning the configured station name, declared under a local coding scheme (99CLARUS) because an AE Title is a site-local identifier (PS3.3 Table 8.8-1). |
| Input retrieval per Input Information Sequence (0040,4021) | **CONFORMANT for co-located deployment**: inputs are retrieved from the same origin by study UID; retrieval URLs (0040,E025) are not consumed. Documented deviation. |
| Appendix V: attribute consistency between the UPS workitem and result objects | **CONFORMANT**. The SEG, SR, PR and RTSTRUCT writers resolve each identity attribute workitem-first and fall back on the study the workitem operates on, which is the order AIW-I Appendix V Table V-1 sets out: its rows are typed "Source Copy" from the UPS Workitem, and the text adds that where the workitem does not carry one, the implementation may take the value from an instance in the Input Information Sequence. A value the workitem carries EMPTY is copied unaltered rather than repaired. Copied attributes: patient identity - Patient Name (0010,0010), Patient ID (0010,0020), Issuer of Patient ID (0010,0021), Patient's Birth Date (0010,0030), Patient's Sex (0010,0040) - Study ID (0020,0010), Accession Number (0008,0050) and Referring Physician's Name (0008,0090); Station Name (0008,1010) is mapped from the Code Meaning of (0040,4028) Performed Station Name Code Sequence (Table V-1 Note 4) and has no fallback; Scheduled Protocol Code Sequence (0040,0008) is mapped from the workitem's (0040,4018); and the requested-procedure rows - Requested Procedure ID (0040,1001), Requested Procedure Description (0032,1060), Requested Procedure Code Sequence (0032,1064), Reason for the Requested Procedure (0040,1002) and Reason for Requested Procedure Code Sequence (0040,100A) - are copied when the workitem carries them. The requested-procedure rows are not written by the RTSTRUCT writer, because the PS3.3 A.19 IOD it implements selects no Request Attributes module and so defines no destination for them; the row above names the writers that do write them. Contributing Equipment Sequence (0018,A001) is not an obligation on result objects: Table V-1 qualifies it "Equal (internally generated) to Performed Station Name Code Sequence (0040,4028)" in the UPS Workitem column, with the result-IOD cell `n.a.`. The DERIVED and STL writers remain study-only and are not part of this row's claim. |
| Consistent Time grouping | **CONFORMANT**. The ITI Consistent Time / Time Client grouping is declared declaratively: time synchronisation is a matter of the site's NTP policy, and no NTP client is embedded in the product. |

### Algorithm provenance on result objects (declared behavior)

Content marking is outside the AIW-I obligations; this paragraph states
Clarus's own declared behavior for result objects, so the target state
lives in one document.

Shipped: every SEG segment carries the DICOM AI-provenance pair -
Segment Algorithm Type (0062,0008) (AUTOMATIC for clinfer-produced
segments) and Segment Algorithm Name (0062,0009) (PS3.3 C.8.20-4) -
the pair a reader uses to tell an automatic result from a traced one
and one model from another.

PLANNED (Type 3 attributes, not AIW-I requirements): model-version
provenance - Segmentation Algorithm Identification Sequence
(0062,0007) with Algorithm Name (0066,0036) and Algorithm Version
(0066,0031) (PS3.3 Table 10-19) when the producer reports a model
version; and, on customer demand, a Contributing Equipment Sequence
(0018,A001) item on PR/SR (SOP Common Table C.12-1; C.12.1.1.5 names
software "such as ... an AI model" as describable equipment).

Content Qualification (0018,9004) is deliberately not part of this
claim: it is Type 3 in SOP Common (Table C.12-1), its values are
PRODUCT / RESEARCH / SERVICE, and its meaning is approval status
("produced with approved hardware and software", C.8.13.2.1.1) - a
site-level regulatory statement, not an AI marker.

## 3. Notification transport (Task Manager)

Clarus exposes **three** notification channels. One is the AIW-I
conformant channel; the other two predate it and remain as documented
extensions.

The conformant channel is the **WebSocket notification connection**
(RAD-109) with Event Reports over it (RAD-87), per PS3.18 8.10.4 and
8.10.6. A user agent opens it with

    GET /dicomweb/subscribers/{requester}

carrying the RFC 6455 upgrade headers (`Upgrade: websocket`,
`Connection: Upgrade`, `Sec-WebSocket-Version: 13`, `Sec-WebSocket-Key`);
`{requester}` is the AE Title of the requesting user agent (PS3.18
Table 8.10.4-1a). The server answers `101 Switching Protocols` with the
`Sec-WebSocket-Accept` value RFC 6455 4.2.2 defines. A request without
the upgrade headers is answered as an ordinary GET on that resource; a
`Sec-WebSocket-Version` other than 13 draws `426 Upgrade Required` with
the supported version advertised (RFC 6455 4.2.2).

Event Reports are JSON text frames. The payload is produced by the same
single serialisation the SSE extension uses, so the two transports
cannot drift. A connection is served only when its `{requester}` holds a
subscription (PS3.18 11.10) that accepts the event, so an open channel
without a subscription receives nothing. Several simultaneous
connections per requester are allowed.
A subscriber may register its subscription WITHOUT a notification URI; it
then receives notifications only over its open WebSocket channel, and UPS
Search remains its durable path.

The two documented extensions, unchanged by this:

- HTTP POST of the created workitem DICOM JSON to the subscriber's
  registered URL (documented in the DICOMweb conformance statement,
  extensions section);
- per-workitem SSE stream at `GET /dicomweb/workitems/{uid}/events`
  (`event: StateChange` frames, keep-alive, per-UID subscriber cap).

All three coexist; the WebSocket channel is the conformant one and the
other two are extensions. A client that needs only conformance uses the
WebSocket channel and ignores the rest.

### The performer's side of the channel

clinfer (the Task Performer) is the user agent that opens the conformant
channel. The order it follows is deliberate and load-bearing: it
registers its subscription FIRST, with no notification URI, and only then
opens the connection - a channel opened before the subscription exists
would be live and permanently silent, which is indistinguishable from a
broken notify path.

On a dropped connection it reconnects with exponential backoff, 1s
doubling to a 30s cap, and re-runs its startup search before resuming.
The channel carries the performer's own step-label filter, so it hears
about the work it is subscribed to and not the whole queue. An Event
Report in a state other than SCHEDULED, a body that is not readable JSON,
or a report carrying no workitem UID are all ordinary traffic on a shared
channel and are ignored: the claim is the ordinary Change State
transaction, and a duplicate or missed notification costs latency, never
correctness.

### Reliability statement

UPS notifications are a latency optimization, not the record of record.
The durable path is UPS Search (11.9) + Retrieve Workitem (11.5) +
Change Workitem State (11.7): a subscriber that was down, restarted, or
disconnected simply re-subscribes and catches up by search; no event is
required to survive the gap. The notification channel is best-effort by
design: a bounded in-memory queue, dropped-and-counted on overflow,
never blocking the workitem write path. Heartbeat and
exponential-backoff reconnection are the subscriber's side of the
contract; the server tolerates channel loss and re-subscription at any
time, and a connection may be reopened at any time. This design
addresses the failure modes documented in SYMPHONY D5.3 (missed updates
after network instability or server restarts).

## 5. Workitem codes

AIW-I mandates no code set (50.4.1.2); Task Requester, Task Manager and
Task Performer agree on codes per deployment. Clarus does not interpret
Scheduled Workitem Code Sequence on the server; clinfer matches
workitems by configuration rules. AIW-I's "400 Normal" priority remark
is a PPS carry-over; Clarus uses the UPS priority enumerated values
(HIGH/MEDIUM/LOW, PS3.3 C.30.1), MEDIUM for the STOW-triggered workitem.

## 6. Known deviations (close before final claim)

D8. Push workflow model of the Task Manager. The manager does not send a
    copy of the workitem to the performer; work is delivered by the
    Triggered Pull model instead, where the performer claims over UPS-RS
    and only the notification is pushed.

## 7. Final (target) conformance and the standard-only viewer story

### The target end-state

Two PLANNED rows remain. The Push workflow model of the Task Manager
(D8) becomes CONFORMANT when a customer needs Push; until then work is
delivered by Pull and Triggered Pull, and every obligation row of the
two claimed actors is CONFORMANT. The model-version provenance items
(Segmentation Algorithm Identification Sequence (0062,0007), a
Contributing Equipment Sequence item on PR/SR) are Type 3 attributes
outside the AIW-I obligation tables and follow the same
customer-demand rule. When Push lands, section 6's deviation list is
empty and this statement is a complete claim with no declared gaps.

### Why a viewer-side "AI tasks" feature is pure standard

The proposed Weasis feature - a dropdown of AI tasks over the archive's
worklist - is UI over transactions the standard already defines. Every
step of the flow maps to a standard transaction, and none of them is a
Clarus extension:

| Viewer step | Standard transaction |
|---|---|
| The archive mints a task when a study arrives | Origin servers can create workitems (PS3.18 11.1: "User agents and origin servers can create Workitems"); the STOW trigger is a product feature built on that right |
| The dropdown lists the study's tasks | UPS Search (11.9) with the study/patient matching keys |
| A doctor requests a task | UPS Create (11.4) - the viewer is a user agent, and 11.1 grants user agents the same right to create workitems |
| The doctor sees the task start and finish | Retrieve Workitem (11.5) / UPS Search; the live path is the Triggered Pull notification (RAD-109/RAD-87) over the standard WebSocket Notification Connection (8.10.4) |
| The results appear with the study | The workitem's Output Information Sequence (0040,4033) names the created SEG/SR/PR/RTSTRUCT instances; the viewer retrieves them with ordinary WADO-RS |

The performer side of that story is clinfer, conformant as a Triggered
Pull Task Performer (section 2). No wire format, no resource and no code
scheme in this flow is Clarus-specific: a Weasis PR implementing the
dropdown works against ANY AIW-I-conformant Task Manager, and any
viewer that speaks UPS-RS + WADO-RS can run the same flow against
Clarus. That is the standard-first reason the proposal needs no
vendor-specific API.

## 8. References

- IHE AIW-I Trial Implementation, Rev 1.1, 2020-08-06
  (IHE_RAD_Suppl_AIW-I.pdf).
- DICOM PS3.18 (2026c) sections 11 (UPS-RS) and 8.10.4 (WebSocket
  Notification Connection).
- DICOM PS3.4 Annex CC (UPS state machine, subscriptions, deletion
  locks).
- SYMPHONY D5.3, ITEA 21026 (11/2025): the only published third-party
  AIW-I triggered-pull implementation known to us; its UPS-RS/WebSocket
  difficulties and remedies are addressed by this statement's design.
  No verbatim excerpts from that (consortium-confidential) deliverable
  are reproduced in this statement.

## 9. Trademarks

DICOM is the registered trademark of the National Electrical
Manufacturers Association (NEMA) for its standards publications
relating to digital communications of medical information. DICOMweb is
a trademark of NEMA. IHE is a trademark of Integrating the Healthcare
Enterprise (IHE International). Clarus is not affiliated with or
endorsed by NEMA or IHE.
