-- Re-price every reward in the currency its scope actually collects.
--
-- `loyalty_reward_items.cost_currency` is a copy of the scope's mode taken when
-- the catalogue was last saved, and nothing kept the two in step. Add rewards
-- before setting the program's mode — the order the dashboard's tabs invite —
-- or switch a program from points to stamps, and every row still says 'points'
-- while the settings say 'visits'.
--
-- Three reads then filtered on the row's own currency, so the whole catalogue
-- vanished: the till offered no rewards, and `cheapest_cost` matched nothing so
-- the card's target fell back to the program default. The dashboard relabelled
-- the same rows with the CURRENT mode, so it displayed a healthy list the whole
-- time, and nothing anywhere reported a disagreement.
--
-- Reads now resolve the currency from the scope, so this is not what makes the
-- feature work again — it is so the stored data stops carrying a claim that is
-- no longer true anywhere else. Amounts are untouched: "espresso, 5" was the
-- admin's number, and it is what the dashboard has been showing them.
UPDATE loyalty_reward_items r
   SET cost_currency = COALESCE(
       -- The scope's own settings, then the org's default, then the column's.
       (SELECT s.mode FROM loyalty_settings s
         WHERE s.org_id = r.org_id
           AND COALESCE(s.branch_id, '00000000-0000-0000-0000-000000000000'::uuid)
             = COALESCE(r.branch_id, '00000000-0000-0000-0000-000000000000'::uuid)),
       (SELECT s.mode FROM loyalty_settings s
         WHERE s.org_id = r.org_id AND s.branch_id IS NULL),
       r.cost_currency)
 WHERE cost_currency IS DISTINCT FROM COALESCE(
       (SELECT s.mode FROM loyalty_settings s
         WHERE s.org_id = r.org_id
           AND COALESCE(s.branch_id, '00000000-0000-0000-0000-000000000000'::uuid)
             = COALESCE(r.branch_id, '00000000-0000-0000-0000-000000000000'::uuid)),
       (SELECT s.mode FROM loyalty_settings s
         WHERE s.org_id = r.org_id AND s.branch_id IS NULL),
       r.cost_currency);
