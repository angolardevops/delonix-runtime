import { buildApp } from "./app.js";

const port = Number(process.env.PORT ?? __PORT__);
const app = buildApp();

app.listen({ port, host: "0.0.0.0" })
  .then((addr) => app.log.info(`listening on ${addr}`))
  .catch((err) => {
    app.log.error(err);
    process.exit(1);
  });
