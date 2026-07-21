# Peashooter php gate image — PHPStan (the type-check gate) + phpactor (cross-file rename), so a
# host running Peashooter needs neither installed. Built locally:
#
#   docker build -f docker/peashooter-php.Dockerfile -t peashooter-php docker/
#
# Both ship as PHARs run on the php runtime; the wrappers expose them as bare `phpstan`/`phpactor`
# commands so the containerized engine resolves them by name (docs/container-gate-spec.md §9b).
FROM php:8.3-cli

# phpactor needs mbstring + pcntl (beyond the bundled tokenizer/ctype/dom/json); phpstan is happy
# with the defaults. libonig-dev is mbstring's build dep.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates libonig-dev \
 && docker-php-ext-install mbstring pcntl \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://github.com/phpstan/phpstan/releases/latest/download/phpstan.phar  -o /usr/local/bin/phpstan.phar \
 && curl -fsSL https://github.com/phpactor/phpactor/releases/latest/download/phpactor.phar -o /usr/local/bin/phpactor.phar \
 && printf '#!/bin/sh\nexec php /usr/local/bin/phpstan.phar "$@"\n'  > /usr/local/bin/phpstan \
 && printf '#!/bin/sh\nexec php /usr/local/bin/phpactor.phar "$@"\n' > /usr/local/bin/phpactor \
 && chmod +x /usr/local/bin/phpstan /usr/local/bin/phpactor
