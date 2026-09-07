-- Can this organisation's logo be recoloured for contrast?
--
-- A card's ground is DERIVED from the logo's dominant colour, so a logo drawn
-- in its own colours is very nearly the colour it is sitting on — the shop that
-- reported this had a blue mark on a blue card. A logo on transparency can be
-- repainted in the card's foreground, which is the one colour already
-- guaranteed to clear AA on that ground; a logo with its background baked in
-- cannot, because every pixel is opaque and the result is a solid rectangle.
-- That one gets a plate instead.
--
-- NULL means "not looked at yet". It is computed at upload, and healed on first
-- read for the logos already uploaded before this existed, so no operator step
-- and no re-upload is needed.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS brand_logo_is_mark boolean;
