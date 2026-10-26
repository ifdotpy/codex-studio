import { post } from "./fixture-api.ts";
import { saved } from "../../src/api.ts";
const api = { post, saved };
export const useServerApi = () => api;
export const useServerStorage = () => localStorage;
