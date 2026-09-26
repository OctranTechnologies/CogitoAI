import { useCallback, useEffect, useRef, useState } from "react";

/**
 * Distance from the bottom, in pixels, within which the view still counts as
 * "at the bottom" and keeps following new activity.
 *
 * A few pixels of slack absorbs sub-pixel layout and the height change caused by
 * a row expanding, which would otherwise look like the user scrolled away.
 */
const BOTTOM_SLACK_PX = 32;

export interface AutoScroll {
  /** Attach to the scrolling element. */
  ref: React.RefObject<HTMLDivElement | null>;
  /** True while new activity is being followed. */
  following: boolean;
  /** Returns to the bottom and resumes following. */
  resume: () => void;
}

/**
 * Follows new activity, but stops the moment the person scrolls away.
 *
 * The rule is deliberately one-directional: content never yanks the viewport
 * back down while the reader is looking at earlier work. Following only resumes
 * when they are already at the bottom, or when they explicitly ask to resume.
 */
export function useAutoScroll(deps: unknown): AutoScroll {
  const ref = useRef<HTMLDivElement | null>(null);
  const [following, setFollowing] = useState(true);

  // Read inside handlers so the scroll listener never needs re-binding and never
  // closes over a stale value.
  const followingRef = useRef(true);
  const setFollowingBoth = useCallback((value: boolean) => {
    followingRef.current = value;
    setFollowing(value);
  }, []);

  const onScroll = useCallback(() => {
    const element = ref.current;
    if (!element) return;
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
    setFollowingBoth(distance <= BOTTOM_SLACK_PX);
  }, [setFollowingBoth]);

  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    element.addEventListener("scroll", onScroll, { passive: true });
    return () => element.removeEventListener("scroll", onScroll);
  }, [onScroll, deps]);

  // New content pins to the bottom only while following.
  useEffect(() => {
    const element = ref.current;
    if (!element || !followingRef.current) return;
    element.scrollTop = element.scrollHeight;
  }, [deps]);

  const resume = useCallback(() => {
    const element = ref.current;
    setFollowingBoth(true);
    if (element) element.scrollTop = element.scrollHeight;
  }, [setFollowingBoth]);

  return { ref, following, resume };
}
