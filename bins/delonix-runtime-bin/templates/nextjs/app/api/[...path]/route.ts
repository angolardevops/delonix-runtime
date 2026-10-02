import { errorResponse, route } from "@/lib/http/route";

export const dynamic = "force-dynamic";

// Any /api path without a Route Handler of its own: answer in the API's error
// shape instead of the HTML not-found page. Not part of the contract
// (api/openapi.yaml), and the drift test knows it.
const notFound = route(() => errorResponse(404, "not_found", "no such route"));

export { notFound as GET, notFound as POST, notFound as PUT, notFound as PATCH, notFound as DELETE };
