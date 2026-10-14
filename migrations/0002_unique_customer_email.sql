-- A business can have only one customer per email (case-insensitive).
CREATE UNIQUE INDEX customers_business_email_uidx ON customers (business_id, lower(email));
