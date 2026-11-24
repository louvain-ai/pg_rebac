# pg_rebac: PGXS packaging for the pure-SQL install, and the PGXN distribution.
#   make               generates dist/pg_rebac--$(VERSION).sql from sql/rebac.sql
#   make install       copies it into $(pg_config --sharedir)/extension (PGXS DATA); there is NO control file,
#                      so it is applied with `psql -f`, not CREATE EXTENSION (see docs / release notes)
#   make dist          pg_rebac-$(VERSION).zip for PGXN, from `git archive` of HEAD plus the generated SQL
VERSION = $(shell sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' META.json | head -1)
SQLFILE = dist/pg_rebac--$(VERSION).sql
DATA    = $(SQLFILE)
EXTRA_CLEAN = dist

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
-include $(PGXS)

all: $(SQLFILE)

$(SQLFILE): sql/rebac.sql scripts/release-sql.sh META.json
	scripts/release-sql.sh $(VERSION) $@

.PHONY: dist
dist: $(SQLFILE)
	rm -rf dist/pg_rebac-$(VERSION) dist/pg_rebac-$(VERSION).zip
	mkdir -p dist/pg_rebac-$(VERSION)
	git archive --format=tar --prefix=pg_rebac-$(VERSION)/ HEAD | tar -x -C dist
	cp $(SQLFILE) dist/pg_rebac-$(VERSION)/
	cd dist && zip -qr pg_rebac-$(VERSION).zip pg_rebac-$(VERSION) && rm -rf pg_rebac-$(VERSION)
	@echo dist/pg_rebac-$(VERSION).zip
