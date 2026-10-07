https://github.com/vishwa24816/Titan-DB

## Example queries (paste into http://localhost:3030, one at a time)

```sql
CREATE TABLE users (id INT, name TEXT, age INT);

INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30);
INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25);
INSERT INTO users (id, name, age) VALUES (3, 'Cara', 35);

SELECT * FROM users;

SELECT * FROM users WHERE age > 25;

SELECT * FROM users WHERE id = 1;

SELECT name, age FROM users ORDER BY age DESC LIMIT 2;

UPDATE users SET name = 'Alicia' WHERE id = 1;

DELETE FROM users WHERE id = 2;

SELECT COUNT(*), AVG(age), MIN(age), MAX(age) FROM users;

SELECT name, age, ROW_NUMBER() OVER (ORDER BY age DESC) FROM users;

SELECT UPPER(name), age + 5 FROM users;

BEGIN; -- via Begin button, then run statements, then Commit/Rollback
```