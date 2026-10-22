import { Global, Module, type OnApplicationShutdown } from "@nestjs/common";

import { AppConfig } from "../config/app-config";
import { AppLogger } from "../logging/app-logger";
import { Cache } from "./cache";
import { InMemoryCache } from "./in-memory-cache";
import { RedisCache } from "./redis-cache";

/**
 * Binds {@link Cache} to the backend the configuration selects: RedisCache when
 * REDIS_URL is set, the in-process InMemoryCache otherwise. Global, so the
 * notes cache-aside layer gets it by type without importing this module, and it
 * closes the backend once on shutdown.
 *
 * The `ioredis` driver is required LAZILY (only on the Redis branch), so a run
 * without REDIS_URL — every unit and e2e test — never loads it or opens a socket.
 */
@Global()
@Module({
  providers: [
    {
      provide: Cache,
      inject: [AppConfig, AppLogger],
      useFactory: async (config: AppConfig, logger: AppLogger): Promise<Cache> => {
        if (config.redisUrl === "") {
          return new InMemoryCache();
        }
        const { Redis } = await import("ioredis");
        const client = new Redis(config.redisUrl, {
          // Fail fast so a lookup falls through to the store instead of hanging;
          // the client reconnects by itself on the next command.
          lazyConnect: true,
          connectTimeout: 2000,
          commandTimeout: 2000,
          maxRetriesPerRequest: 1,
          enableOfflineQueue: false,
        });
        return new RedisCache(client, logger);
      },
    },
  ],
  exports: [Cache],
})
export class CacheModule implements OnApplicationShutdown {
  constructor(private readonly cache: Cache) {}

  async onApplicationShutdown(): Promise<void> {
    await this.cache.close();
  }
}
